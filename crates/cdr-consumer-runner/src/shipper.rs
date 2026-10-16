//! Ships each [`CdrObservation`] to VictoriaLogs as one `/insert/jsonline`
//! line, the way the load generators ship their call-outcome records: the
//! observation verbatim in `_msg`, `_time` from its end, `source`
//! `cdr-consumer` the stream field (ADR-0049 §3.8).
//!
//! Observations queue in a bounded channel; a batch goes out every second or
//! at [`BATCH`] lines, and a batch whose post fails is posted again until it
//! lands. A full queue drops the new observation and counts it, so the
//! consumer never stalls on the store.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use metric_catalogue::{Family, Labels};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::observation::CdrObservation;

/// Lines per post.
pub const BATCH: usize = 1000;
/// Observations waiting to be shipped before new ones are dropped.
pub const QUEUE: usize = 100_000;

/// When a batch goes out and how a failed post is retried.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// A batch goes out this long after its first observation, or full.
    pub flush: Duration,
    /// A failed post is posted again after this.
    pub retry: Duration,
    /// A post that has not answered by then failed.
    pub post_timeout: Duration,
}

impl Pacing {
    pub const DEFAULT: Pacing = Pacing {
        flush: Duration::from_secs(1),
        retry: Duration::from_secs(2),
        post_timeout: Duration::from_secs(10),
    };
}
const INSERT: &str = "/insert/jsonline?_stream_fields=source&_msg_field=_msg&_time_field=_time";

/// Observations shipped.
pub const SHIPPED: Family = Family::counter(
    "cdr_observations_shipped_total",
    Labels::None,
    "CDR observations posted to the observation store",
);

/// Observations dropped at a full queue.
pub const DROPPED: Family = Family::counter(
    "cdr_observations_dropped_total",
    Labels::None,
    "CDR observations dropped because the shipping queue was full",
);

/// Posts that failed and were retried.
pub const POST_FAILURES: Family = Family::counter(
    "cdr_observation_post_failures_total",
    Labels::None,
    "posts of a batch of CDR observations that failed (the batch is posted again)",
);

/// Payloads the observation view could not read: CDRs the join never sees.
pub const UNREAD: Family = Family::counter(
    "cdr_observations_unread_total",
    Labels::None,
    "CDR payloads the observation view could not read, so no observation was shipped for them",
);

/// Every family [`ShipMetrics::prometheus_text`] renders, in order.
pub const FAMILIES: &[Family] = &[SHIPPED, DROPPED, POST_FAILURES, UNREAD];

#[derive(Default)]
pub struct ShipMetrics {
    shipped: AtomicU64,
    dropped: AtomicU64,
    post_failures: AtomicU64,
    unread: AtomicU64,
}

impl ShipMetrics {
    pub fn prometheus_text(&self) -> String {
        let mut s = String::new();
        SHIPPED.render_value(&mut s, self.shipped.load(Ordering::Relaxed));
        DROPPED.render_value(&mut s, self.dropped.load(Ordering::Relaxed));
        POST_FAILURES.render_value(&mut s, self.post_failures.load(Ordering::Relaxed));
        UNREAD.render_value(&mut s, self.unread.load(Ordering::Relaxed));
        s
    }
}

/// Where observations go: host and port of an `http://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// Reads `http://host[:port][/]`; `Err` names what is wrong.
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("CDR_OBSERVATION_URL must be http://host[:port], got {url:?}"))?
            .trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') {
            return Err(format!("CDR_OBSERVATION_URL must be http://host[:port], got {url:?}"));
        }
        match rest.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse()
                    .map_err(|_| format!("CDR_OBSERVATION_URL: bad port in {url:?}"))?;
                Ok(Endpoint { host: host.to_string(), port })
            }
            None => Ok(Endpoint { host: rest.to_string(), port: 80 }),
        }
    }
}

/// The sending half: offers never wait.
#[derive(Clone)]
pub struct ObservationSink {
    tx: mpsc::Sender<CdrObservation>,
    metrics: Arc<ShipMetrics>,
}

impl ObservationSink {
    /// Queues one observation; a full queue drops it and counts it.
    pub fn offer(&self, obs: CdrObservation) {
        if self.tx.try_send(obs).is_err() {
            self.metrics.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts a payload the observation view could not read.
    pub fn note_unread(&self) {
        self.metrics.unread.fetch_add(1, Ordering::Relaxed);
    }
}

/// Starts the shipping task on the current runtime.
pub fn spawn(endpoint: Endpoint, metrics: Arc<ShipMetrics>) -> ObservationSink {
    spawn_with(endpoint, metrics, Pacing::DEFAULT, QUEUE)
}

/// [`spawn`] with its pacing and queue bound given.
pub fn spawn_with(
    endpoint: Endpoint,
    metrics: Arc<ShipMetrics>,
    pacing: Pacing,
    queue: usize,
) -> ObservationSink {
    let (tx, rx) = mpsc::channel(queue);
    tokio::spawn(ship(endpoint, rx, metrics.clone(), pacing));
    ObservationSink { tx, metrics }
}

/// The VictoriaLogs line of one observation.
pub fn line(obs: &CdrObservation) -> String {
    let msg = serde_json::to_string(obs).expect("an observation serializes");
    serde_json::json!({
        "_msg": msg,
        "_time": rfc3339_ms(obs.end_ms),
        "source": "cdr-consumer",
        "call_id": obs.call_id,
    })
    .to_string()
}

/// `ms` since the Unix epoch as `YYYY-MM-DDThh:mm:ss.mmmZ`.
pub fn rfc3339_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Days to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

async fn post(endpoint: &Endpoint, body: &str) -> std::io::Result<bool> {
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
    let head = format!(
        "POST {INSERT} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/stream+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.host,
        endpoint.port,
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await?;
    let status = String::from_utf8_lossy(&resp);
    Ok(status.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2')))
}

async fn ship(
    endpoint: Endpoint,
    mut rx: mpsc::Receiver<CdrObservation>,
    metrics: Arc<ShipMetrics>,
    pacing: Pacing,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![line(&first)];
        let deadline = tokio::time::Instant::now() + pacing.flush;
        while batch.len() < BATCH {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(obs)) => batch.push(line(&obs)),
                Ok(None) | Err(_) => break,
            }
        }
        let body = batch.join("\n") + "\n";
        loop {
            match tokio::time::timeout(pacing.post_timeout, post(&endpoint, &body)).await {
                Ok(Ok(true)) => break,
                outcome => {
                    metrics.post_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(?outcome, host = %endpoint.host, "CDR observation post failed; retrying");
                    tokio::time::sleep(pacing.retry).await;
                }
            }
        }
        metrics.shipped.fetch_add(batch.len() as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_http_url_names_its_host_and_port() {
        assert_eq!(
            Endpoint::parse("http://172.20.0.1:9428"),
            Ok(Endpoint { host: "172.20.0.1".into(), port: 9428 })
        );
        assert_eq!(Endpoint::parse("http://logs/"), Ok(Endpoint { host: "logs".into(), port: 80 }));
        for bad in ["https://x:1", "http://x:1/insert", "http://", "http://x:port"] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_time_is_rfc3339_in_utc() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_792_102_669_188), "2026-10-15T22:17:49.188Z");
        assert_eq!(rfc3339_ms(951_782_400_001), "2000-02-29T00:00:00.001Z");
    }

    #[test]
    fn a_line_carries_the_observation_verbatim_and_its_stream_field() {
        let obs = CdrObservation::new("c-run1@h".into(), 1000, Some(2000), 5000, 1);
        let v: serde_json::Value = serde_json::from_str(&line(&obs)).unwrap();
        assert_eq!(v["source"], "cdr-consumer");
        assert_eq!(v["call_id"], "c-run1@h");
        assert_eq!(v["_time"], "1970-01-01T00:00:05.000Z");
        let msg: serde_json::Value = serde_json::from_str(v["_msg"].as_str().unwrap()).unwrap();
        assert_eq!(msg["duration_ms"], 3000);
        assert_eq!(msg["schema"], "cdr-observation.v1");
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_counts() {
        let (tx, _rx) = mpsc::channel(1);
        let metrics = Arc::new(ShipMetrics::default());
        let sink = ObservationSink { tx, metrics: metrics.clone() };
        for _ in 0..3 {
            sink.offer(CdrObservation::new("c".into(), 0, None, 0, 1));
        }
        assert!(metrics.prometheus_text().contains("cdr_observations_dropped_total 2"));
    }

    /// One HTTP request off `stream`: its headers, then `Content-Length` bytes of body.
    async fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .map_or(0, |v| v.trim().parse().unwrap());
                if body.len() >= len || n == 0 {
                    return text;
                }
            }
        }
    }

    #[tokio::test]
    async fn a_failed_post_is_posted_again_and_counted_shipped_once_it_lands() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for status in ["500 Internal Server Error", "204 No Content"] {
                let (mut s, _) = listener.accept().await.unwrap();
                bodies.push(read_request(&mut s).await);
                s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            }
            bodies
        });
        let metrics = Arc::new(ShipMetrics::default());
        let pacing = Pacing {
            flush: Duration::from_millis(50),
            retry: Duration::from_millis(20),
            post_timeout: Duration::from_secs(5),
        };
        let sink =
            spawn_with(Endpoint { host: "127.0.0.1".into(), port }, metrics.clone(), pacing, 16);
        for i in 0..3 {
            sink.offer(CdrObservation::new(format!("c{i}"), 0, None, 1000, 1));
        }
        let bodies = tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
        for b in &bodies {
            assert!(b.starts_with(&format!("POST {INSERT} HTTP/1.1")), "{b}");
            assert_eq!(b.matches("\"source\":\"cdr-consumer\"").count(), 3, "{b}");
        }
        for _ in 0..100 {
            if metrics.prometheus_text().contains("cdr_observations_shipped_total 3") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let text = metrics.prometheus_text();
        assert!(text.contains("cdr_observations_shipped_total 3"), "{text}");
        assert!(text.contains("cdr_observation_post_failures_total 1"), "{text}");
    }
}
