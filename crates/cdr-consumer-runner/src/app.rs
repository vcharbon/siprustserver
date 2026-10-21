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
use lapin::options::BasicAckOptions;
use probe_http::{ProbeRoutes, ProbeServer, ProbeState};

use crate::counting::{Metrics, Payload};
use crate::link::{LinkMetrics, Target};
use crate::observation::ObservationView;
use crate::shipper::{self, Endpoint, ObservationSink, ShipMetrics, Shipped};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The `/metrics` and `/healthz` routes: the consumer's counts, its link's
/// state and the shipper's counts, and readiness that is always ready (the
/// consumer takes no traffic; `cdr_amqp_consuming` tells its link's state).
fn probe_routes(
    metrics: Arc<Metrics>,
    link: Arc<LinkMetrics>,
    ship: Arc<ShipMetrics>,
) -> ProbeRoutes {
    ProbeRoutes {
        metrics: Arc::new(move || crate::metrics_body(&metrics, &link, &ship)),
        ready: Arc::new(|| ProbeState::Ready),
        heap: None,
        vrrp: None,
        admin: None,
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

/// The bound of one AMQP setup, from resolving the broker to the consume registered.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// An ack the client refused: its channel failed or the delivery was already acked.
fn ack_refused() -> io::Error {
    io::Error::other("the CDR ack was refused: the channel failed or the acker was used")
}

/// One link → consume pass. Returns `Err` on any AMQP fault so the outer loop
/// reconnects; runs forever on success (the consume stream is endless).
async fn consume(
    target: &Target,
    link_metrics: &Arc<LinkMetrics>,
    consumer: &Consumer,
) -> Result<(), lapin::Error> {
    let mut link = target.open(link_metrics).await?;
    while let Some(delivery) = link.deliveries().next().await {
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
    tracing::info!(
        node = observe::node(),
        state = "stream_closed",
        queue = %target.queue,
        "AMQP connection"
    );
    Ok(())
}

/// The consumer, reading observations with `view`. Runs until the process ends.
pub async fn run(view: Arc<dyn ObservationView>) {
    let target = Target {
        url: env_or("CDR_AMQP_URL", "amqp://guest:guest@rabbitmq:5672/%2f"),
        queue: env_or("CDR_QUEUE", "cdr"),
        max_len: env_or("CDR_QUEUE_MAX_LEN", "100000").parse().expect("CDR_QUEUE_MAX_LEN"),
        setup_timeout: SETUP_TIMEOUT,
    };
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

    let link_metrics = Arc::new(LinkMetrics::default());
    let routes = probe_routes(consumer.metrics.clone(), link_metrics.clone(), ship.clone());
    let probe = ProbeServer::start(addr, routes)
        .await
        .unwrap_or_else(|e| panic!("failed to bind CDR_METRICS_LISTEN {addr}: {e}"));
    tracing::info!(addr = %probe.addr(), "metrics server listening (/metrics)");

    // Reconnect loop: the broker may be down at startup or bounce mid-run. CDRs
    // missed while disconnected are bounded by the broker's own x-max-length.
    loop {
        if let Err(e) = consume(&target, &link_metrics, &consumer).await {
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

    async fn get(addr: std::net::SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).await.unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }

    #[tokio::test]
    async fn the_probe_routes_serve_the_counts_and_liveness() {
        let c = consumer(None);
        c.deliver(RECORD);
        let ship = Arc::new(ShipMetrics::default());
        let routes = probe_routes(c.metrics.clone(), Arc::new(LinkMetrics::default()), ship);
        let probe = ProbeServer::start("127.0.0.1:0".parse().unwrap(), routes).await.unwrap();
        let metrics = get(probe.addr(), "/metrics").await;
        assert!(
            metrics.contains("200 OK") && metrics.contains("cdr_consumed_total 1"),
            "{metrics}"
        );
        assert!(get(probe.addr(), "/healthz").await.contains("200 OK"));
    }
}
