//! `cdr-consumer-runner` — the dedicated CDR metrics consumer.
//!
//! Drains the RabbitMQ CDR queue the b2bua workers publish to (one JSON
//! [`CdrRecord`] per terminated call) and turns it into two Prometheus counters:
//!
//! - `cdr_consumed_total` — total number of CDRs consumed
//! - `cdr_call_duration_ms_total` — summed call duration across all CDRs,
//!   i.e. Σ (terminated_at − created_at)
//!
//! plus `cdr_parse_errors_total` for malformed payloads. It does NOT persist
//! anything — it is telemetry only — so on restart the counters reset to 0 and
//! vmagent/Grafana see the usual counter-reset (handled by `rate()`/`increase()`).
//!
//! It speaks AMQP over the same `lapin` + tokio shims the producer uses, and
//! serves `/metrics` + `/healthz` on one port with a hand-rolled HTTP responder
//! (mirroring `b2bua-runner`'s metrics server — no framework dependency).
//!
//! ## Config (env)
//! - `CDR_AMQP_URL`       AMQP URI                 (default `amqp://guest:guest@rabbitmq:5672/%2f`)
//! - `CDR_QUEUE`          queue name to consume    (default `cdr`)
//! - `CDR_QUEUE_MAX_LEN`  broker `x-max-length`    (default `100000`; MUST match the producer)
//! - `CDR_METRICS_LISTEN` Prometheus listen addr   (default `0.0.0.0:9093`)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    Connection, ConnectionProperties,
};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The only fields we read off a CDR; serde_json ignores the rest.
#[derive(Deserialize)]
struct CdrDurationView {
    created_at: i64,
    terminated_at: i64,
}

#[derive(Default)]
struct Metrics {
    consumed: AtomicU64,
    duration_ms: AtomicU64,
    parse_errors: AtomicU64,
}

impl Metrics {
    fn prometheus_text(&self) -> String {
        let consumed = self.consumed.load(Ordering::Relaxed);
        let duration_ms = self.duration_ms.load(Ordering::Relaxed);
        let parse_errors = self.parse_errors.load(Ordering::Relaxed);
        let mut s = String::new();
        s.push_str("# HELP cdr_consumed_total total CDRs consumed from the RabbitMQ queue\n");
        s.push_str("# TYPE cdr_consumed_total counter\n");
        s.push_str(&format!("cdr_consumed_total {consumed}\n"));
        s.push_str("# HELP cdr_call_duration_ms_total summed call duration in ms across all consumed CDRs (terminated_at - created_at)\n");
        s.push_str("# TYPE cdr_call_duration_ms_total counter\n");
        s.push_str(&format!("cdr_call_duration_ms_total {duration_ms}\n"));
        s.push_str("# HELP cdr_parse_errors_total CDR payloads that failed to decode\n");
        s.push_str("# TYPE cdr_parse_errors_total counter\n");
        s.push_str(&format!("cdr_parse_errors_total {parse_errors}\n"));
        s
    }
}

/// Hand-rolled Prometheus exposition + liveness server (mirrors b2bua-runner).
async fn serve_metrics(addr: std::net::SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    eprintln!("cdr-consumer metrics on http://{}/metrics", listener.local_addr()?);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.starts_with("GET /metrics") {
                ("200 OK", metrics.prometheus_text())
            } else if req.starts_with("GET /healthz") {
                ("200 OK", "ok\n".to_string())
            } else {
                ("404 Not Found", String::new())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

/// One connect → declare → consume pass. Returns `Err` on any AMQP fault so the
/// outer loop reconnects; runs forever on success (the consume stream is endless).
async fn consume(
    url: &str,
    queue: &str,
    max_len: i64,
    metrics: &Metrics,
) -> Result<(), lapin::Error> {
    let props = ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);
    let conn = Connection::connect(url, props).await?;
    let chan = conn.create_channel().await?;

    // Declare the SAME queue the producer declares (durable + bounded), so the
    // two declarations agree argument-for-argument whichever side wins the race.
    let mut args = FieldTable::default();
    if max_len > 0 {
        args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
        args.insert(
            "x-overflow".into(),
            AMQPValue::LongString(LongString::from("drop-head")),
        );
    }
    chan.queue_declare(
        queue,
        QueueDeclareOptions {
            durable: true,
            ..Default::default()
        },
        args,
    )
    .await?;

    let mut consumer = chan
        .basic_consume(
            queue,
            "cdr-consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    eprintln!("cdr-consumer: consuming queue {queue:?} on {url}");

    while let Some(delivery) = consumer.next().await {
        let delivery = delivery?;
        match serde_json::from_slice::<CdrDurationView>(&delivery.data) {
            Ok(view) => {
                let dur = (view.terminated_at - view.created_at).max(0) as u64;
                metrics.consumed.fetch_add(1, Ordering::Relaxed);
                metrics.duration_ms.fetch_add(dur, Ordering::Relaxed);
            }
            Err(e) => {
                metrics.parse_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("cdr-consumer: parse error: {e}");
            }
        }
        // Ack regardless: a malformed record is counted, not redelivered forever.
        delivery.ack(BasicAckOptions::default()).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let url = env_or("CDR_AMQP_URL", "amqp://guest:guest@rabbitmq:5672/%2f");
    let queue = env_or("CDR_QUEUE", "cdr");
    let max_len: i64 = env_or("CDR_QUEUE_MAX_LEN", "100000")
        .parse()
        .expect("CDR_QUEUE_MAX_LEN");
    let metrics_listen = env_or("CDR_METRICS_LISTEN", "0.0.0.0:9093");
    let addr: std::net::SocketAddr = metrics_listen
        .parse()
        .unwrap_or_else(|e| panic!("bad CDR_METRICS_LISTEN {metrics_listen:?}: {e}"));

    let metrics = Arc::new(Metrics::default());

    let m = metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(addr, m).await {
            eprintln!("cdr-consumer metrics server error: {e}");
        }
    });

    // Reconnect loop: the broker may be down at startup or bounce mid-run. CDRs
    // missed while disconnected are bounded by the broker's own x-max-length.
    loop {
        if let Err(e) = consume(&url, &queue, max_len, &metrics).await {
            eprintln!("cdr-consumer: AMQP error: {e}; reconnecting in 2s");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
