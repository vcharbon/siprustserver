//! The RabbitMQ CDR sink a runner composes from env ([`RabbitMqCdrSettings`]):
//! [`RabbitMqCdrWriter`] publishes one record per terminated call, in the
//! format its [`CdrEncoder`] produces (default [`JsonRecordEncoder`]); without
//! a URL there is no sink, and [`crate::RunnerBase::deps`] keeps its
//! discarding default.
//!
//! ## Buffering
//! The writer sits behind the `BufferedCdrWriter` [`crate::RunnerBase::deps`]
//! installs, so the hot-path `write()` enqueues non-blocking with
//! drop-on-overload at `B2BUA_CDR_QUEUE` depth, and a single drainer task calls
//! this writer serially (the channel guarded below is uncontended). A queue the
//! writer owns ([`CdrQueueDeclare::Own`]) is declared with `x-max-length` +
//! `x-overflow=drop-head`: a consumer that falls behind loses the oldest
//! records instead of growing the broker.
//!
//! ## Failure handling
//! Delivery is best-effort telemetry, never on the call's critical path. The
//! connection is established lazily on the first write; an encode, connect or
//! publish failure counts one dropped record, and a publish failure drops the
//! connection so the next record reconnects.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{CdrEncoder, CdrRecord, CdrWriter, JsonRecordEncoder};
use b2bua::metrics::B2buaMetrics;
use call::Call;
use lapin::{
    options::{BasicPublishOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use tokio::sync::Mutex;

/// How the writer declares its destination queue, `B2BUA_CDR_RABBITMQ_DECLARE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdrQueueDeclare {
    /// `own` (default): the writer declares the queue durable, bounded by
    /// `x-max-length` with `x-overflow=drop-head` (`B2BUA_CDR_RABBITMQ_MAX_LEN`,
    /// default 100000; `0` disables the bound). Every other declarer of the
    /// queue must pass the same arguments or the broker refuses both.
    Own { max_len: i64 },
    /// `existing`: the broker already holds the queue; the writer declares it
    /// passively (fails if absent) and never states its arguments, so a queue
    /// with arguments of its own (a quorum queue, a dead-letter exchange) is
    /// published to as it stands.
    Existing,
}

/// The RabbitMQ CDR sink's env grammar, the one statement of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RabbitMqCdrSettings {
    /// AMQP URI (`amqp://user:pass@host:5672/vhost`), `B2BUA_CDR_RABBITMQ_URL`.
    pub url: String,
    /// Destination queue, `B2BUA_CDR_RABBITMQ_QUEUE` (default `cdr`).
    pub queue: String,
    /// How the queue is declared, `B2BUA_CDR_RABBITMQ_DECLARE` + `_MAX_LEN`.
    pub declare: CdrQueueDeclare,
}

impl RabbitMqCdrSettings {
    /// Reads the grammar through `get`; `Ok(None)` when the URL is unset or
    /// blank. `Err` names the variable when `MAX_LEN` is not an integer, when
    /// `DECLARE` is neither `own` nor `existing` (blank = `own`), or when
    /// `MAX_LEN` is set beside `DECLARE=existing` (the broker owns the bound).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(url) = get("B2BUA_CDR_RABBITMQ_URL").filter(|u| !u.trim().is_empty()) else {
            return Ok(None);
        };
        let queue = get("B2BUA_CDR_RABBITMQ_QUEUE").unwrap_or_else(|| "cdr".to_string());
        let raw_max = get("B2BUA_CDR_RABBITMQ_MAX_LEN");
        let raw_declare = get("B2BUA_CDR_RABBITMQ_DECLARE").filter(|v| !v.trim().is_empty());
        let declare = match raw_declare.as_deref() {
            None | Some("own") => {
                let raw_max = raw_max.unwrap_or_else(|| "100000".to_string());
                let max_len = raw_max.parse().map_err(|e| {
                    format!("B2BUA_CDR_RABBITMQ_MAX_LEN must be an integer, got {raw_max:?}: {e}")
                })?;
                CdrQueueDeclare::Own { max_len }
            }
            Some("existing") => {
                if let Some(raw_max) = raw_max {
                    return Err(format!(
                        "B2BUA_CDR_RABBITMQ_MAX_LEN={raw_max:?} has no effect with \
                         B2BUA_CDR_RABBITMQ_DECLARE=existing: the broker holds the queue \
                         and its arguments; unset it"
                    ));
                }
                CdrQueueDeclare::Existing
            }
            Some(other) => {
                return Err(format!(
                    "B2BUA_CDR_RABBITMQ_DECLARE must be `own` or `existing`, got {other:?}"
                ));
            }
        };
        Ok(Some(Self { url, queue, declare }))
    }

    /// The writer these settings describe, publishing the default
    /// [`JsonRecordEncoder`] record and recording into `metrics` (the registry
    /// the core exports, so `cdr_written_total` and `cdr_dropped_total` count
    /// its publishes).
    pub fn into_sink(self, metrics: &B2buaMetrics) -> Arc<dyn CdrWriter> {
        self.into_sink_with_encoder(Arc::new(JsonRecordEncoder), metrics)
    }

    /// [`Self::into_sink`] publishing the bytes `encoder` produces.
    pub fn into_sink_with_encoder(
        self,
        encoder: Arc<dyn CdrEncoder>,
        metrics: &B2buaMetrics,
    ) -> Arc<dyn CdrWriter> {
        Arc::new(RabbitMqCdrWriter::new(self, encoder, metrics.clone()))
    }
}

/// The RabbitMQ sink the settings read through `get` select, publishing the
/// default record and recording into `metrics`; `Ok(None)` when no URL is set.
pub fn rabbitmq_cdr_sink_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    metrics: &B2buaMetrics,
) -> Result<Option<Arc<dyn CdrWriter>>, String> {
    rabbitmq_cdr_sink_from_lookup_with_encoder(get, Arc::new(JsonRecordEncoder), metrics)
}

/// [`rabbitmq_cdr_sink_from_lookup`] publishing the bytes `encoder` produces.
pub fn rabbitmq_cdr_sink_from_lookup_with_encoder(
    get: impl Fn(&str) -> Option<String>,
    encoder: Arc<dyn CdrEncoder>,
    metrics: &B2buaMetrics,
) -> Result<Option<Arc<dyn CdrWriter>>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| s.into_sink_with_encoder(encoder, metrics)))
}

/// Publishes terminated-call CDRs, encoded by its [`CdrEncoder`], onto an AMQP
/// queue.
pub struct RabbitMqCdrWriter {
    settings: RabbitMqCdrSettings,
    encoder: Arc<dyn CdrEncoder>,
    /// Lazily (re)established connection + channel. Held together so the
    /// connection's IO task stays alive (dropping `Connection` closes it).
    /// `None` until the first successful connect or after a publish error.
    chan: Mutex<Option<(Connection, Channel)>>,
    /// Shared b2bua registry: publish success bumps `cdr_written_total`, every
    /// failure path (encode/connect/publish) bumps `cdr_dropped_total`.
    metrics: B2buaMetrics,
}

impl RabbitMqCdrWriter {
    pub fn new(
        settings: RabbitMqCdrSettings,
        encoder: Arc<dyn CdrEncoder>,
        metrics: B2buaMetrics,
    ) -> Self {
        Self { settings, encoder, chan: Mutex::new(None), metrics }
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

    /// The queue declaration [`CdrQueueDeclare`] states.
    fn declaration(&self) -> (QueueDeclareOptions, FieldTable) {
        let mut args = FieldTable::default();
        match self.settings.declare {
            CdrQueueDeclare::Own { max_len } => {
                if max_len > 0 {
                    // Drop the OLDEST record on overflow so a stalled consumer
                    // never grows the broker without limit.
                    args.insert("x-max-length".into(), AMQPValue::LongLongInt(max_len));
                    args.insert(
                        "x-overflow".into(),
                        AMQPValue::LongString(LongString::from("drop-head")),
                    );
                }
                (QueueDeclareOptions { durable: true, ..Default::default() }, args)
            }
            CdrQueueDeclare::Existing => {
                (QueueDeclareOptions { passive: true, ..Default::default() }, args)
            }
        }
    }

    /// Connect over the same tokio runtime everything else uses, then declare
    /// the destination queue per [`Self::declaration`].
    async fn connect(&self) -> Result<(Connection, Channel), lapin::Error> {
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let conn = Connection::connect(&self.settings.url, props).await?;
        let chan = conn.create_channel().await?;
        let (options, args) = self.declaration();
        chan.queue_declare(&self.settings.queue, options, args).await?;
        Ok((conn, chan))
    }
}

#[async_trait]
impl CdrWriter for RabbitMqCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        let Some(payload) = self.payload(call, terminated_at) else {
            return;
        };

        let mut guard = self.chan.lock().await;
        if guard.is_none() {
            match self.connect().await {
                Ok(c) => *guard = Some(c),
                Err(e) => {
                    self.metrics.bump_cdr_dropped();
                    tracing::warn!(url = %self.settings.url, error = %e, "CDR broker connect failed");
                    return;
                }
            }
        }
        let chan = &guard.as_ref().expect("connected above").1;
        // Default exchange, routing key = queue name (direct to the queue).
        // `persistent` so a broker restart keeps queued records on a durable
        // queue.
        match chan
            .basic_publish(
                "",
                &self.settings.queue,
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default().with_delivery_mode(2),
            )
            .await
        {
            Ok(_confirm) => {
                self.metrics.bump_cdr_written();
            }
            Err(e) => {
                self.metrics.bump_cdr_dropped();
                tracing::warn!(error = %e, "CDR publish failed; will reconnect");
                // Drop the channel/connection so the next write reconnects.
                *guard = None;
            }
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
        RabbitMqCdrSettings { url: "amqp://h/%2f".into(), queue: "cdr".into(), declare }
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
    fn unset_or_blank_url_selects_no_sink() {
        assert_eq!(RabbitMqCdrSettings::from_lookup(lookup(&[])), Ok(None));
        assert_eq!(
            RabbitMqCdrSettings::from_lookup(lookup(&[("B2BUA_CDR_RABBITMQ_URL", "  ")])),
            Ok(None)
        );
        let metrics = B2buaMetrics::new();
        assert!(rabbitmq_cdr_sink_from_lookup(lookup(&[]), &metrics).expect("ok").is_none());
    }

    #[test]
    fn url_alone_takes_the_default_queue_and_owns_it_bounded() {
        let s = RabbitMqCdrSettings::from_lookup(lookup(&[(
            "B2BUA_CDR_RABBITMQ_URL",
            "amqp://guest:guest@rabbitmq:5672/%2f",
        )]));
        assert_eq!(
            s,
            Ok(Some(RabbitMqCdrSettings {
                url: "amqp://guest:guest@rabbitmq:5672/%2f".into(),
                queue: "cdr".into(),
                declare: OWN_DEFAULT,
            }))
        );
    }

    #[test]
    fn queue_and_bound_are_read_from_their_variables() {
        let s = RabbitMqCdrSettings::from_lookup(lookup(&[
            ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
            ("B2BUA_CDR_RABBITMQ_QUEUE", "cdr-lane"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "0"),
        ]))
        .expect("ok")
        .expect("some");
        assert_eq!(
            (s.queue.as_str(), s.declare),
            ("cdr-lane", CdrQueueDeclare::Own { max_len: 0 })
        );
    }

    #[test]
    fn a_non_integer_bound_is_refused_naming_its_variable() {
        let e = RabbitMqCdrSettings::from_lookup(lookup(&[
            ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "lots"),
        ]))
        .expect_err("a non-integer bound must refuse boot");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_MAX_LEN"), "msg was: {e}");
        assert!(
            RabbitMqCdrSettings::from_lookup(lookup(&[
                ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
                ("B2BUA_CDR_RABBITMQ_MAX_LEN", " 5"),
            ]))
            .is_err(),
            "a padded bound is not an integer either"
        );
    }

    #[test]
    fn declare_own_and_blank_select_the_owned_queue() {
        for v in ["own", "", "  "] {
            let s = RabbitMqCdrSettings::from_lookup(lookup(&[
                ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
                ("B2BUA_CDR_RABBITMQ_DECLARE", v),
                ("B2BUA_CDR_RABBITMQ_MAX_LEN", "7"),
            ]))
            .expect("ok")
            .expect("some");
            assert_eq!(s.declare, CdrQueueDeclare::Own { max_len: 7 }, "DECLARE={v:?}");
        }
    }

    #[test]
    fn declare_existing_selects_the_broker_held_queue() {
        let s = RabbitMqCdrSettings::from_lookup(lookup(&[
            ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
            ("B2BUA_CDR_RABBITMQ_QUEUE", "held"),
            ("B2BUA_CDR_RABBITMQ_DECLARE", "existing"),
        ]))
        .expect("ok")
        .expect("some");
        assert_eq!((s.queue.as_str(), s.declare), ("held", CdrQueueDeclare::Existing));
    }

    #[test]
    fn an_unknown_declare_is_refused_naming_its_variable_and_values() {
        for v in ["passive", "Own", " own"] {
            let e = RabbitMqCdrSettings::from_lookup(lookup(&[
                ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
                ("B2BUA_CDR_RABBITMQ_DECLARE", v),
            ]))
            .expect_err("an unknown declare policy must refuse boot");
            assert!(e.contains("B2BUA_CDR_RABBITMQ_DECLARE"), "msg was: {e}");
            assert!(e.contains("own") && e.contains("existing"), "msg was: {e}");
        }
    }

    #[test]
    fn a_bound_beside_declare_existing_is_refused_naming_both_variables() {
        let e = RabbitMqCdrSettings::from_lookup(lookup(&[
            ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
            ("B2BUA_CDR_RABBITMQ_DECLARE", "existing"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "100000"),
        ]))
        .expect_err("a bound on a broker-held queue has no effect and must refuse boot");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_MAX_LEN"), "msg was: {e}");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_DECLARE"), "msg was: {e}");
    }

    #[test]
    fn an_owned_queue_is_declared_durable_and_bounded_drop_head() {
        let (opts, args) = writer(OWN_DEFAULT, Arc::new(JsonRecordEncoder)).declaration();
        assert!(opts.durable && !opts.passive);
        let inner = args.inner();
        assert_eq!(inner.get("x-max-length"), Some(&AMQPValue::LongLongInt(100_000)));
        assert_eq!(
            inner.get("x-overflow"),
            Some(&AMQPValue::LongString(LongString::from("drop-head")))
        );
    }

    #[test]
    fn an_owned_unbounded_queue_is_declared_durable_without_arguments() {
        let (opts, args) =
            writer(CdrQueueDeclare::Own { max_len: 0 }, Arc::new(JsonRecordEncoder)).declaration();
        assert!(opts.durable && !opts.passive);
        assert!(args.inner().is_empty());
    }

    #[test]
    fn a_broker_held_queue_is_declared_passively_without_arguments() {
        let (opts, args) =
            writer(CdrQueueDeclare::Existing, Arc::new(JsonRecordEncoder)).declaration();
        assert!(opts.passive, "an existing queue is never (re)declared");
        assert!(args.inner().is_empty(), "no argument of the broker's queue is restated");
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

    /// The selected sink publishes: a write against an unparsable broker URI
    /// fails before any socket is opened and counts one dropped record, which
    /// the discarding default never does.
    #[tokio::test]
    async fn a_set_url_selects_the_broker_sink_not_the_discarding_default() {
        let metrics = B2buaMetrics::new();
        let sink = rabbitmq_cdr_sink_from_lookup(
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
        let sink = rabbitmq_cdr_sink_from_lookup_with_encoder(
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
