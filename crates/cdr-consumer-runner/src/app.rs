//! The consumer process: drain the queue, count each delivery, ship its
//! observation when `CDR_OBSERVATION_URL` is set, serve `/metrics` and
//! `/healthz`. A product with its own CDR payload runs [`run`] with its own
//! [`ObservationView`].
//!
//! ## Config (env)
//! - `CDR_AMQP_URL`        AMQP URI                 (default `amqp://guest:guest@rabbitmq:5672/%2f`)
//! - `CDR_QUEUE`           queue name to consume    (default `cdr`)
//! - `CDR_QUEUE_MAX_LEN`   broker `x-max-length`    (default `100000`; MUST match the producer)
//! - `CDR_METRICS_LISTEN`  Prometheus listen addr   (default `0.0.0.0:9093`)
//! - `CDR_PAYLOAD`         `json` or `opaque`       (default `json`; see `counting::Payload`)
//! - `CDR_OBSERVATION_URL` `http://host:port` of the VictoriaLogs the observations go to (unset: none shipped)

use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    uri::AMQPUri,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

use crate::counting::{Metrics, Payload};
use crate::observation::ObservationView;
use crate::shipper::{self, Endpoint, ObservationSink, ShipMetrics, Shipped};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Hand-rolled Prometheus exposition + liveness server (mirrors b2bua-runner).
async fn serve_metrics(
    addr: std::net::SocketAddr,
    metrics: Arc<Metrics>,
    ship: Arc<ShipMetrics>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "metrics server listening (/metrics)");
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        let ship = ship.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let (status, body) = if req.starts_with("GET /metrics") {
                ("200 OK", crate::metrics_body(&metrics, &ship))
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

struct Consumer {
    payload: Payload,
    metrics: Arc<Metrics>,
    view: Arc<dyn ObservationView>,
    sink: Option<ObservationSink>,
}

impl Consumer {
    /// Counts one delivery and offers its observation; the signal of an
    /// offered observation fires once it has landed.
    fn deliver(&self, data: &[u8]) -> Option<Shipped> {
        if let Err(e) = self.metrics.count(self.payload, data) {
            tracing::warn!(error = %e, "CDR payload parse error");
        }
        let sink = self.sink.as_ref()?;
        match self.view.observe(data) {
            Some(obs) => Some(sink.offer(obs)),
            None => {
                sink.note_unread();
                None
            }
        }
    }
}

/// How long one connect (resolution, TCP, TLS) may take before the pass fails.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// An ack the client refused: its channel failed or the delivery was already acked.
fn ack_refused() -> io::Error {
    io::Error::other("the CDR ack was refused: the channel failed or the acker was used")
}

/// One connect → declare → consume pass. Returns `Err` on any AMQP fault so the
/// outer loop reconnects; runs forever on success (the consume stream is endless).
async fn consume(
    url: &str,
    queue: &str,
    max_len: i64,
    consumer: &Consumer,
) -> Result<(), lapin::Error> {
    tracing::info!(node = observe::node(), state = "connecting", %url, "AMQP connection");
    let uri: AMQPUri = url.parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let connected = amqp_connect::connect_uri(&uri, Instant::now() + CONNECT_TIMEOUT).await?;
    let conn = amqp_connect::open_over(uri, connected.stream).await?;
    tracing::info!(node = observe::node(), state = "connected", %url, "AMQP connection");
    let chan = conn.create_channel().await?;

    // Declare the SAME queue the producer declares (durable + bounded), so the
    // two declarations agree argument-for-argument whichever side wins the race.
    let mut args = FieldTable::default();
    if max_len > 0 {
        args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
        args.insert("x-overflow".into(), AMQPValue::LongString(LongString::from("drop-head")));
    }
    let durable = QueueDeclareOptions { durable: true, ..Default::default() };
    chan.queue_declare(queue.into(), durable, args).await?;

    let mut deliveries = chan
        .basic_consume(
            queue.into(),
            "cdr-consumer".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    tracing::info!(node = observe::node(), state = "consuming", %queue, %url, "AMQP connection");

    while let Some(delivery) = deliveries.next().await {
        let delivery = delivery?;
        // Ack regardless of the payload: a malformed record is counted, not
        // redelivered forever. A shipped observation is acked once it landed,
        // so a consumer gone before then leaves its CDR to the broker.
        match consumer.deliver(&delivery.data) {
            None => {
                if !delivery.ack(BasicAckOptions::default()).await? {
                    return Err(ack_refused().into());
                }
            }
            Some(shipped) => {
                let acker = delivery.acker;
                tokio::spawn(async move {
                    let _ = shipped.await;
                    let failed = match acker.ack(BasicAckOptions::default()).await {
                        Ok(true) => None,
                        Ok(false) => Some(ack_refused().to_string()),
                        Err(e) => Some(e.to_string()),
                    };
                    if let Some(error) = failed {
                        tracing::warn!(%error, "CDR ack after its observation shipped failed");
                    }
                });
            }
        }
    }
    // The consumer stream ended without an error — the broker closed it.
    tracing::info!(node = observe::node(), state = "stream_closed", %queue, "AMQP connection");
    Ok(())
}

/// The consumer, reading observations with `view`. Runs until the process ends.
pub async fn run(view: Arc<dyn ObservationView>) {
    let url = env_or("CDR_AMQP_URL", "amqp://guest:guest@rabbitmq:5672/%2f");
    let queue = env_or("CDR_QUEUE", "cdr");
    let max_len: i64 = env_or("CDR_QUEUE_MAX_LEN", "100000").parse().expect("CDR_QUEUE_MAX_LEN");
    let metrics_listen = env_or("CDR_METRICS_LISTEN", "0.0.0.0:9093");
    let payload = Payload::from_env_value(std::env::var("CDR_PAYLOAD").ok().as_deref())
        .unwrap_or_else(|e| panic!("{e}"));
    let addr: std::net::SocketAddr = metrics_listen
        .parse()
        .unwrap_or_else(|e| panic!("bad CDR_METRICS_LISTEN {metrics_listen:?}: {e}"));
    let ship = Arc::new(ShipMetrics::default());
    let sink =
        std::env::var("CDR_OBSERVATION_URL").ok().filter(|u| !u.trim().is_empty()).map(|u| {
            shipper::spawn(
                Endpoint::parse(u.trim()).unwrap_or_else(|e| panic!("{e}")),
                ship.clone(),
            )
        });
    let consumer = Consumer { payload, metrics: Arc::new(Metrics::default()), view, sink };

    let (m, s) = (consumer.metrics.clone(), ship.clone());
    tokio::spawn(async move {
        if let Err(e) = serve_metrics(addr, m, s).await {
            tracing::error!(error = %e, "metrics server error");
        }
    });

    // Reconnect loop: the broker may be down at startup or bounce mid-run. CDRs
    // missed while disconnected are bounded by the broker's own x-max-length.
    loop {
        if let Err(e) = consume(&url, &queue, max_len, &consumer).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::JsonRecordView;
    use crate::shipper::{spawn_with, Pacing};

    const RECORD: &[u8] = br#"{"call_ref":"r","created_at":1000,"terminated_at":3000,"a_leg":{"call_id":"c1","from_tag":"f","state":"terminated"},"b_legs":[],"events":[],"decision_log":[],"termination":null}"#;

    fn consumer(sink: Option<ObservationSink>) -> Consumer {
        Consumer {
            payload: Payload::Json,
            metrics: Arc::new(Metrics::default()),
            view: Arc::new(JsonRecordView),
            sink,
        }
    }

    /// A sink whose shipper posts nowhere reachable for 10 s: what is offered stays queued.
    fn parked_sink(ship: &Arc<ShipMetrics>, queue: usize) -> ObservationSink {
        let pacing = Pacing {
            flush: Duration::from_secs(10),
            retry: Duration::from_secs(10),
            post_timeout: Duration::from_secs(10),
        };
        spawn_with(
            crate::shipper::Endpoint { host: "127.0.0.1".into(), port: 9 },
            ship.clone(),
            pacing,
            queue,
        )
    }

    #[tokio::test]
    async fn a_readable_delivery_is_offered_and_an_unreadable_one_is_counted() {
        let ship = Arc::new(ShipMetrics::default());
        let c = consumer(Some(parked_sink(&ship, 1)));
        c.deliver(RECORD);
        c.deliver(b"<cdr/>");
        // The second readable one finds the one-slot queue taken: dropped, never waited on.
        c.deliver(RECORD);
        c.deliver(RECORD);
        let text = ship.prometheus_text();
        assert!(text.contains("cdr_observations_unread_total 1"), "{text}");
        // The shipper task has not run between the synchronous deliveries: the
        // first stays queued, the next two find the queue full.
        assert!(text.contains("cdr_observations_dropped_total 2"), "{text}");
        assert!(c.metrics.prometheus_text().contains("cdr_consumed_total 3"));
    }

    #[test]
    fn without_a_sink_a_delivery_is_only_counted() {
        let c = consumer(None);
        c.deliver(RECORD);
        c.deliver(b"<cdr/>");
        let text = c.metrics.prometheus_text();
        assert!(
            text.contains("cdr_consumed_total 1") && text.contains("cdr_parse_errors_total 1"),
            "{text}"
        );
    }
}
