//! The RabbitMQ CDR sink a runner composes from env ([`RabbitMqCdrSettings`]):
//! [`RabbitMqCdrWriter`] publishes one record per terminated call, in the
//! format its [`CdrEncoder`] produces (default [`JsonRecordEncoder`]); without
//! a URL there is no sink, and [`crate::RunnerBase::deps`] keeps its
//! discarding default.
//!
//! ## Contract
//! Every record handed to [`RabbitMqCdrWriter::write`] ends, once, in exactly
//! one of `cdr_written_total` (the broker acked it on a confirm-mode channel)
//! or `cdr_dropped_total` (refused by the encoder, no connection, reconnect
//! backoff, window full past the publish bound, publish failed, nacked,
//! returned unroutable, no confirm within the confirm bound, or the connection
//! ended with it unconfirmed). A record the broker held and later lost is
//! outside what the sink can see.
//!
//! ## Bounded waits
//! Delivery is best-effort telemetry and never slows the call path: the writer
//! sits behind the `BufferedCdrWriter` [`crate::RunnerBase::deps`] installs
//! (non-blocking enqueue, drop-on-overload at `B2BUA_CDR_QUEUE`, one drainer
//! calling this writer serially), and every wait of that drainer on the broker
//! is bounded ([`CdrDeliveryBounds`]): the connect, a window slot plus the
//! publish hand-off, and, off the drainer, each confirm. At most `window`
//! publishes await their confirm at once. A broker that is down, refusing,
//! hung or flow-controlling therefore turns into dropped records, never into
//! a stalled drainer or a growing memory. After a failed connection the next
//! attempt waits out a doubling backoff, and records arriving meanwhile are
//! dropped; a connection that delivered reconnects at once when it ends.
//!
//! A queue the writer owns ([`CdrQueueDeclare::Own`]) is declared with
//! `x-max-length` + `x-overflow=drop-head`: a consumer that falls behind loses
//! the oldest records instead of growing the broker. A returned publish ends
//! the connection, so the next one declares the queue again.

mod backoff;
mod session;
mod settings;
mod socket_kill;

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{CdrEncoder, CdrRecord, CdrWriter, JsonRecordEncoder};
use b2bua::metrics::B2buaMetrics;
use call::Call;
use tokio::sync::Mutex;
use tokio::time::{timeout, Instant};

use backoff::Backoff;
use session::Session;
pub use settings::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, MAX_WAIT_MS, MAX_WINDOW,
};

impl RabbitMqCdrSettings {
    /// The writer these settings describe, publishing the default
    /// [`JsonRecordEncoder`] record and recording into `metrics` (the registry
    /// the core exports, so `cdr_written_total` and `cdr_dropped_total` count
    /// its publishes). Every default entry point of the kit comes through here.
    pub fn into_writer(self, metrics: &B2buaMetrics) -> RabbitMqCdrWriter {
        self.into_writer_with_encoder(Arc::new(JsonRecordEncoder), metrics)
    }

    /// [`Self::into_writer`] publishing the bytes `encoder` produces.
    pub fn into_writer_with_encoder(
        self,
        encoder: Arc<dyn CdrEncoder>,
        metrics: &B2buaMetrics,
    ) -> RabbitMqCdrWriter {
        RabbitMqCdrWriter::new(self, encoder, metrics.clone())
    }
}

/// The RabbitMQ writer the settings read through `get` select, publishing the
/// default record and recording into `metrics`; `Ok(None)` when no URL is set.
pub fn rabbitmq_cdr_writer_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    metrics: &B2buaMetrics,
) -> Result<Option<RabbitMqCdrWriter>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| s.into_writer(metrics)))
}

/// [`rabbitmq_cdr_writer_from_lookup`] publishing the bytes `encoder` produces.
pub fn rabbitmq_cdr_writer_from_lookup_with_encoder(
    get: impl Fn(&str) -> Option<String>,
    encoder: Arc<dyn CdrEncoder>,
    metrics: &B2buaMetrics,
) -> Result<Option<RabbitMqCdrWriter>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| s.into_writer_with_encoder(encoder, metrics)))
}

/// Publishes terminated-call CDRs, encoded by its [`CdrEncoder`], onto an AMQP
/// queue under publisher confirms (the module's contract).
pub struct RabbitMqCdrWriter {
    settings: RabbitMqCdrSettings,
    encoder: Arc<dyn CdrEncoder>,
    /// The current connection and the backoff guarding the next one; taken by
    /// the one drainer, so uncontended.
    link: Mutex<Link>,
    /// Shared b2bua registry `cdr_written_total` / `cdr_dropped_total` count in.
    metrics: B2buaMetrics,
}

struct Link {
    /// `None` until the first connect, and after a session ended.
    session: Option<Session>,
    backoff: Backoff,
}

impl RabbitMqCdrWriter {
    pub fn new(
        settings: RabbitMqCdrSettings,
        encoder: Arc<dyn CdrEncoder>,
        metrics: B2buaMetrics,
    ) -> Self {
        let backoff = Backoff::new(settings.bounds.backoff_min, settings.bounds.backoff_max);
        Self { settings, encoder, link: Mutex::new(Link { session: None, backoff }), metrics }
    }

    /// The settings this writer publishes under.
    pub fn settings(&self) -> &RabbitMqCdrSettings {
        &self.settings
    }

    /// The bytes published for `call`; `None`, counted as one dropped record,
    /// when the encoder refuses it.
    fn payload(&self, call: &Call, terminated_at: i64) -> Option<Vec<u8>> {
        match self.encoder.encode(call, terminated_at) {
            Ok(payload) => Some(payload),
            Err(e) => {
                // A record the encoder refuses is a bug, not a transient fault;
                // count it as dropped and move on (never poison the drainer).
                self.metrics.bump_cdr_dropped();
                tracing::warn!(call_ref = %call.call_ref, error = %e, "CDR encode failed");
                None
            }
        }
    }

    /// The live session, connecting one within the connect bound when there
    /// is none and the backoff allows; `None` when no session can publish now.
    async fn session<'a>(&self, link: &'a mut Link) -> Option<&'a Session> {
        if let Some(ended_at) = link.session.as_ref().and_then(Session::ended_at) {
            // The backoff counts from the end, not from this record.
            if link.session.take().is_some_and(|s| s.delivered()) {
                link.backoff.recovered();
            } else {
                link.backoff.failed(ended_at);
            }
        }
        if link.session.is_none() {
            if !link.backoff.ready(Instant::now()) {
                return None;
            }
            let opened = timeout(
                self.settings.bounds.connect_timeout,
                Session::open(&self.settings, &self.metrics),
            )
            .await;
            match opened {
                Ok(Ok(session)) => link.session = Some(session),
                Ok(Err(e)) => {
                    let wait = link.backoff.failed(Instant::now());
                    tracing::warn!(error = %e, retry_in_ms = wait.as_millis() as u64, "CDR broker connect failed");
                    return None;
                }
                Err(_) => {
                    let wait = link.backoff.failed(Instant::now());
                    tracing::warn!(
                        timeout_ms = self.settings.bounds.connect_timeout.as_millis() as u64,
                        retry_in_ms = wait.as_millis() as u64,
                        "CDR broker connect timed out"
                    );
                    return None;
                }
            }
        }
        link.session.as_ref()
    }
}

#[async_trait]
impl CdrWriter for RabbitMqCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        let Some(payload) = self.payload(call, terminated_at) else {
            return;
        };
        let mut link = self.link.lock().await;
        let Some(session) = self.session(&mut link).await else {
            self.metrics.bump_cdr_dropped();
            return;
        };
        let deadline = Instant::now() + self.settings.bounds.publish_timeout;
        if let Err(refused) = session.publish(&self.settings.queue, &payload, deadline).await {
            self.metrics.bump_cdr_dropped();
            tracing::debug!(?refused, "CDR record dropped");
        }
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        // Not a test sink — records live in the broker, not in process memory.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use b2bua::cdr::{build_record, CdrEncodeError};
    use b2bua::config::B2buaConfig;
    use b2bua::initial_invite::build_initial_call;
    use call::Call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn a_call() -> Call {
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-sink\r\n\
            Max-Forwards: 70\r\n\
            From: <sip:alice@example.com>;tag=alicetag\r\n\
            To: <sip:bob@example.com>\r\n\
            Call-ID: sink-probe@10.0.0.9\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:alice@10.0.0.9:5060>\r\n\
            Content-Length: 0\r\n\r\n";
        let req = match CustomParser::new().parse(raw.as_bytes()).expect("parse") {
            SipMessage::Request(r) => r,
            _ => panic!("expected a request"),
        };
        let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        build_initial_call(&req, SocketAddr::from(([10, 0, 0, 9], 5060)), &cfg, 1_000)
    }

    fn settings(declare: CdrQueueDeclare) -> RabbitMqCdrSettings {
        RabbitMqCdrSettings {
            url: "amqp://h/%2f".into(),
            queue: "cdr".into(),
            declare,
            bounds: CdrDeliveryBounds::default(),
        }
    }

    fn writer(declare: CdrQueueDeclare, encoder: Arc<dyn CdrEncoder>) -> RabbitMqCdrWriter {
        RabbitMqCdrWriter::new(settings(declare), encoder, B2buaMetrics::new())
    }

    /// Refuses every call; counts its invocations.
    #[derive(Default)]
    struct RefusingEncoder {
        calls: AtomicUsize,
    }

    impl CdrEncoder for RefusingEncoder {
        fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("refused".into())
        }
    }

    const OWN_DEFAULT: CdrQueueDeclare = CdrQueueDeclare::Own { max_len: 100_000 };

    #[test]
    fn an_unset_url_selects_no_writer() {
        let metrics = B2buaMetrics::new();
        assert!(rabbitmq_cdr_writer_from_lookup(lookup(&[]), &metrics).expect("ok").is_none());
    }

    /// The default writer publishes the bytes `serde_json::to_vec` gave over
    /// `build_record` before the encoder seam, in order and skew-clamped alike.
    #[test]
    fn the_default_writer_publishes_the_json_record_byte_for_byte() {
        let metrics = B2buaMetrics::new();
        let settings = settings(OWN_DEFAULT);
        let w = RabbitMqCdrWriter::new(settings, Arc::new(JsonRecordEncoder), metrics.clone());
        let call = a_call();
        for terminated_at in [2_000, 500] {
            let expected = serde_json::to_vec(&build_record(&call, terminated_at)).unwrap();
            assert_eq!(w.payload(&call, terminated_at), Some(expected), "t={terminated_at}");
        }
        assert_eq!(metrics.cdr_dropped_total(), 0);
    }

    #[test]
    fn an_encode_refusal_counts_one_dropped_record() {
        let metrics = B2buaMetrics::new();
        let enc = Arc::new(RefusingEncoder::default());
        let w = RabbitMqCdrWriter::new(settings(OWN_DEFAULT), enc.clone(), metrics.clone());
        assert_eq!(w.payload(&a_call(), 2_000), None);
        assert_eq!(enc.calls.load(Ordering::SeqCst), 1);
        assert_eq!((metrics.cdr_dropped_total(), metrics.cdr_written_total()), (1, 0));
    }

    /// The native record of [`a_call`], created at 1000 and terminated at 2000,
    /// as the RabbitMQ sink has always published it.
    const GOLDEN: &str = concat!(
        r#"{"call_ref":"w0|sink-probe@10.0.0.9|alicetag","created_at":1000,"terminated_at":2000,"#,
        r#""a_leg":{"call_id":"sink-probe@10.0.0.9","from_tag":"alicetag","state":"trying"},"#,
        r#""b_legs":[],"events":[{"type":"invite_received","timestamp":1000,"leg_id":"a","#,
        r#""status_code":null,"reason":null,"decision_ordinal":0}],"decision_log":[],"#,
        r#""termination":null}"#
    );

    /// The same call discharged at 500, before its creation: clamped and flagged.
    const GOLDEN_SKEWED: &str = concat!(
        r#"{"call_ref":"w0|sink-probe@10.0.0.9|alicetag","created_at":1000,"terminated_at":1000,"#,
        r#""clock_skew_clamped":true,"#,
        r#""a_leg":{"call_id":"sink-probe@10.0.0.9","from_tag":"alicetag","state":"trying"},"#,
        r#""b_legs":[],"events":[{"type":"invite_received","timestamp":1000,"leg_id":"a","#,
        r#""status_code":null,"reason":null,"decision_ordinal":0}],"decision_log":[],"#,
        r#""termination":null}"#
    );

    fn assert_publishes_the_golden_record(w: &RabbitMqCdrWriter) {
        let call = a_call();
        for (t, golden) in [(2_000, GOLDEN), (500, GOLDEN_SKEWED)] {
            let got = w.payload(&call, t).expect("the default record encodes");
            assert_eq!(String::from_utf8(got).expect("utf-8"), golden, "terminated_at={t}");
        }
    }

    /// The default settings entry point publishes the native record, byte for
    /// byte.
    #[test]
    fn into_writer_publishes_the_golden_native_record() {
        let w = settings(OWN_DEFAULT).into_writer(&B2buaMetrics::new());
        assert_publishes_the_golden_record(&w);
    }

    /// The default lookup entry point, which `RunnerBase::rabbitmq_cdr_sink_from_env`
    /// reads the process env through, publishes the native record, byte for byte.
    #[test]
    fn the_default_lookup_publishes_the_golden_native_record() {
        let w = rabbitmq_cdr_writer_from_lookup(
            lookup(&[("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f")]),
            &B2buaMetrics::new(),
        )
        .expect("ok")
        .expect("some");
        assert_publishes_the_golden_record(&w);
    }

    /// Records the `terminated_at` each call is encoded with.
    #[derive(Default)]
    struct RecordingEncoder {
        seen: std::sync::Mutex<Vec<i64>>,
    }

    impl CdrEncoder for RecordingEncoder {
        fn encode(&self, _call: &Call, terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
            self.seen.lock().unwrap().push(terminated_at);
            Ok(Vec::new())
        }
    }

    /// The writer hands the encoder the raw discharge stamp, even one that
    /// precedes the call's creation (cross-node skew); clamping is the
    /// encoder's.
    #[test]
    fn the_writer_passes_a_skewed_terminated_at_through_unclamped() {
        let enc = Arc::new(RecordingEncoder::default());
        let w = writer(OWN_DEFAULT, enc.clone());
        let call = a_call();
        assert_eq!(call.created_at, 1_000);
        assert_eq!(w.payload(&call, 500), Some(Vec::new()));
        assert_eq!(*enc.seen.lock().unwrap(), vec![500]);
    }

    /// The selected sink publishes: a write against an unparsable broker URI
    /// fails before any socket is opened and counts one dropped record, which
    /// the discarding default never does.
    #[tokio::test]
    async fn a_set_url_selects_the_broker_sink_not_the_discarding_default() {
        let metrics = B2buaMetrics::new();
        let sink = rabbitmq_cdr_writer_from_lookup(
            lookup(&[("B2BUA_CDR_RABBITMQ_URL", "not an amqp uri")]),
            &metrics,
        )
        .expect("ok")
        .expect("a set URL must select the RabbitMQ sink");
        sink.write(&a_call(), 2_000).await;
        assert_eq!(metrics.cdr_dropped_total(), 1, "the publish attempt must be counted");
        assert_eq!(metrics.cdr_written_total(), 0);
    }

    /// The sink selected with an encoder encodes through it: a refusing encoder
    /// is called once per write and each refusal is one dropped record.
    #[tokio::test]
    async fn a_sink_selected_with_an_encoder_encodes_through_it() {
        let metrics = B2buaMetrics::new();
        let enc = Arc::new(RefusingEncoder::default());
        let sink = rabbitmq_cdr_writer_from_lookup_with_encoder(
            lookup(&[("B2BUA_CDR_RABBITMQ_URL", "not an amqp uri")]),
            enc.clone(),
            &metrics,
        )
        .expect("ok")
        .expect("a set URL must select the RabbitMQ sink");
        sink.write(&a_call(), 2_000).await;
        sink.write(&a_call(), 3_000).await;
        assert_eq!(enc.calls.load(Ordering::SeqCst), 2);
        assert_eq!((metrics.cdr_dropped_total(), metrics.cdr_written_total()), (2, 0));
    }
}
