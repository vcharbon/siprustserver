//! `cdr-consumer-runner` — the dedicated CDR metrics consumer.
//!
//! Drains the RabbitMQ CDR queue the b2bua workers publish to (one record per
//! terminated call) and turns it into Prometheus counters
//! ([`counting`](cdr_consumer_runner::counting)):
//!
//! - `cdr_consumed_total` — total number of CDRs consumed
//! - `cdr_call_duration_ms_total` — summed call duration across all CDRs,
//!   i.e. Σ (terminated_at − created_at), 0 in `opaque` mode — and
//!   `cdr_parse_errors_total` for payloads that do not decode.
//!
//! `opaque` mode counts deliveries whatever their format. It does NOT persist
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
//! - `CDR_PAYLOAD`        `json` or `opaque`       (default `json`; see `counting::Payload`)

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    Connection, ConnectionProperties,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use cdr_consumer_runner::counting::{Metrics, Payload};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Hand-rolled Prometheus exposition + liveness server (mirrors b2bua-runner).
async fn serve_metrics(addr: std::net::SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "metrics server listening (/metrics)");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.starts_with("GET /metrics") {
                ("200 OK", cdr_consumer_runner::metrics_body(&metrics))
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
    payload: Payload,
    metrics: &Metrics,
) -> Result<(), lapin::Error> {
    let props = ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);
    tracing::info!(node = observe::node(), state = "connecting", %url, "AMQP connection");
    let conn = Connection::connect(url, props).await?;
    tracing::info!(node = observe::node(), state = "connected", %url, "AMQP connection");
    let chan = conn.create_channel().await?;

    // Declare the SAME queue the producer declares (durable + bounded), so the
    // two declarations agree argument-for-argument whichever side wins the race.
    let mut args = FieldTable::default();
    if max_len > 0 {
        args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
        args.insert("x-overflow".into(), AMQPValue::LongString(LongString::from("drop-head")));
    }
    chan.queue_declare(queue, QueueDeclareOptions { durable: true, ..Default::default() }, args)
        .await?;

    let mut consumer = chan
        .basic_consume(queue, "cdr-consumer", BasicConsumeOptions::default(), FieldTable::default())
        .await?;
    tracing::info!(
        node = observe::node(),
        state = "consuming",
        %queue,
        %url,
        "AMQP connection"
    );

    while let Some(delivery) = consumer.next().await {
        let delivery = delivery?;
        if let Err(e) = metrics.count(payload, &delivery.data) {
            tracing::warn!(error = %e, "CDR payload parse error");
        }
        // Ack regardless: a malformed record is counted, not redelivered forever.
        delivery.ack(BasicAckOptions::default()).await?;
    }
    // The consumer stream ended without an error — the broker closed it.
    tracing::info!(
        node = observe::node(),
        state = "stream_closed",
        %queue,
        "AMQP connection"
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    // Subscriber first (ADR-0026); the guard drains the log writer at exit.
    let _observe = observe::init_production("cdr-consumer-runner");

    let url = env_or("CDR_AMQP_URL", "amqp://guest:guest@rabbitmq:5672/%2f");
    let queue = env_or("CDR_QUEUE", "cdr");
    let max_len: i64 = env_or("CDR_QUEUE_MAX_LEN", "100000").parse().expect("CDR_QUEUE_MAX_LEN");
    let metrics_listen = env_or("CDR_METRICS_LISTEN", "0.0.0.0:9093");
    let payload = Payload::from_env_value(std::env::var("CDR_PAYLOAD").ok().as_deref())
        .unwrap_or_else(|e| panic!("{e}"));
    let addr: std::net::SocketAddr = metrics_listen
        .parse()
        .unwrap_or_else(|e| panic!("bad CDR_METRICS_LISTEN {metrics_listen:?}: {e}"));

    let metrics = Arc::new(Metrics::default());

    let m = metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(addr, m).await {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    // Reconnect loop: the broker may be down at startup or bounce mid-run. CDRs
    // missed while disconnected are bounded by the broker's own x-max-length.
    loop {
        if let Err(e) = consume(&url, &queue, max_len, payload, &metrics).await {
            tracing::warn!(
                node = observe::node(),
                state = "disconnected",
                error = %e,
                retry_in_sec = 2,
                "AMQP connection"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
