//! The RabbitMQ CDR sink a runner composes from env: `B2BUA_CDR_RABBITMQ_URL`
//! set → [`RabbitMqCdrWriter`], publishing one JSON [`CdrRecord`] per
//! terminated call; unset → no sink, and [`crate::RunnerBase::deps`] keeps its
//! discarding default.
//!
//! ## Buffering
//! The writer sits behind the `BufferedCdrWriter` [`crate::RunnerBase::deps`]
//! installs, so the hot-path `write()` enqueues non-blocking with
//! drop-on-overload at `B2BUA_CDR_QUEUE` depth, and a single drainer task calls
//! this writer serially (the channel guarded below is uncontended). The broker
//! queue is declared with `x-max-length` + `x-overflow=drop-head`: a consumer
//! that falls behind loses the oldest records instead of growing the broker.
//!
//! ## Failure handling
//! Delivery is best-effort telemetry, never on the call's critical path. The
//! connection is established lazily on the first write; a connect, serialize
//! or publish failure counts one dropped record, and a publish failure drops
//! the connection so the next record reconnects.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::{build_record, CdrRecord, CdrWriter};
use b2bua::metrics::B2buaMetrics;
use call::Call;
use lapin::{
    options::{BasicPublishOptions, QueueDeclareOptions},
    types::{AMQPValue, FieldTable, LongString},
    BasicProperties, Channel, Connection, ConnectionProperties,
};
use tokio::sync::Mutex;

/// The RabbitMQ sink's env grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RabbitMqCdrSettings {
    /// AMQP URI (`amqp://user:pass@host:5672/vhost`), `B2BUA_CDR_RABBITMQ_URL`.
    pub url: String,
    /// Destination queue, `B2BUA_CDR_RABBITMQ_QUEUE` (default `cdr`).
    pub queue: String,
    /// Broker-side queue cap (`x-max-length`), `B2BUA_CDR_RABBITMQ_MAX_LEN`
    /// (default 100000; `0` disables the bound).
    pub max_len: i64,
}

impl RabbitMqCdrSettings {
    /// Reads the grammar through `get`; `Ok(None)` when the URL is unset or
    /// blank, `Err` naming the variable when `MAX_LEN` is not an integer.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(url) = get("B2BUA_CDR_RABBITMQ_URL").filter(|u| !u.trim().is_empty()) else {
            return Ok(None);
        };
        let queue = get("B2BUA_CDR_RABBITMQ_QUEUE").unwrap_or_else(|| "cdr".to_string());
        let raw_max = get("B2BUA_CDR_RABBITMQ_MAX_LEN").unwrap_or_else(|| "100000".to_string());
        let max_len = raw_max.trim().parse().map_err(|e| {
            format!("B2BUA_CDR_RABBITMQ_MAX_LEN must be an integer, got {raw_max:?}: {e}")
        })?;
        Ok(Some(Self { url, queue, max_len }))
    }
}

/// The RabbitMQ sink the settings read through `get` select, recording into
/// `metrics` (the registry the core exports, so `cdr_written_total` and
/// `cdr_dropped_total` count its publishes); `Ok(None)` when no URL is set.
pub fn rabbitmq_cdr_sink_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    metrics: &B2buaMetrics,
) -> Result<Option<Arc<dyn CdrWriter>>, String> {
    Ok(RabbitMqCdrSettings::from_lookup(get)?.map(|s| {
        Arc::new(RabbitMqCdrWriter::new(s.url, s.queue, s.max_len, metrics.clone()))
            as Arc<dyn CdrWriter>
    }))
}

/// Publishes terminated-call CDRs as JSON onto an AMQP queue.
pub struct RabbitMqCdrWriter {
    url: String,
    queue: String,
    /// Broker-side queue cap (`x-max-length`). `0` disables the bound.
    max_len: i64,
    /// Lazily (re)established connection + channel. Held together so the
    /// connection's IO task stays alive (dropping `Connection` closes it).
    /// `None` until the first successful connect or after a publish error.
    chan: Mutex<Option<(Connection, Channel)>>,
    /// Shared b2bua registry: publish success bumps `cdr_written_total`, every
    /// failure path (serialize/connect/publish) bumps `cdr_dropped_total`.
    metrics: B2buaMetrics,
}

impl RabbitMqCdrWriter {
    /// `url` is an AMQP URI (`amqp://user:pass@host:5672/vhost`); `queue` is the
    /// destination queue name; `max_len` bounds the broker queue (0 = unbounded).
    pub fn new(url: String, queue: String, max_len: i64, metrics: B2buaMetrics) -> Self {
        Self { url, queue, max_len, chan: Mutex::new(None), metrics }
    }

    /// Connect over the same tokio runtime everything else uses, then declare the
    /// durable, length-bounded destination queue (idempotent — must match the
    /// consumer's declaration argument-for-argument or the broker errors).
    async fn connect(&self) -> Result<(Connection, Channel), lapin::Error> {
        let props = ConnectionProperties::default()
            .with_executor(tokio_executor_trait::Tokio::current())
            .with_reactor(tokio_reactor_trait::Tokio);
        let conn = Connection::connect(&self.url, props).await?;
        let chan = conn.create_channel().await?;
        let mut args = FieldTable::default();
        if self.max_len > 0 {
            // Bound the broker queue; drop the OLDEST record on overflow so a
            // stalled consumer never grows the broker without limit.
            args.insert("x-max-length".into(), AMQPValue::LongLongInt(self.max_len));
            args.insert("x-overflow".into(), AMQPValue::LongString(LongString::from("drop-head")));
        }
        chan.queue_declare(
            &self.queue,
            QueueDeclareOptions { durable: true, ..Default::default() },
            args,
        )
        .await?;
        Ok((conn, chan))
    }
}

#[async_trait]
impl CdrWriter for RabbitMqCdrWriter {
    async fn write(&self, call: &Call, terminated_at: i64) {
        let record = build_record(call, terminated_at);
        let payload = match serde_json::to_vec(&record) {
            Ok(p) => p,
            Err(e) => {
                // A record that won't serialize is a bug, not a transient fault;
                // count it as dropped and move on (never poison the drainer).
                self.metrics.bump_cdr_dropped();
                tracing::warn!(call_ref = %record.call_ref, error = %e, "CDR serialize failed");
                return;
            }
        };

        let mut guard = self.chan.lock().await;
        if guard.is_none() {
            match self.connect().await {
                Ok(c) => *guard = Some(c),
                Err(e) => {
                    self.metrics.bump_cdr_dropped();
                    tracing::warn!(url = %self.url, error = %e, "CDR broker connect failed");
                    return;
                }
            }
        }
        let chan = &guard.as_ref().expect("connected above").1;
        // Default exchange, routing key = queue name (direct to the queue).
        // `persistent` so a broker restart keeps queued records (paired with the
        // durable queue declared above).
        match chan
            .basic_publish(
                "",
                &self.queue,
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
    use b2bua::config::B2buaConfig;
    use b2bua::initial_invite::build_initial_call;
    use call::Call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use std::collections::HashMap;
    use std::net::SocketAddr;

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
    fn url_alone_takes_the_default_queue_and_bound() {
        let s = RabbitMqCdrSettings::from_lookup(lookup(&[(
            "B2BUA_CDR_RABBITMQ_URL",
            "amqp://guest:guest@rabbitmq:5672/%2f",
        )]));
        assert_eq!(
            s,
            Ok(Some(RabbitMqCdrSettings {
                url: "amqp://guest:guest@rabbitmq:5672/%2f".into(),
                queue: "cdr".into(),
                max_len: 100_000,
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
        assert_eq!((s.queue.as_str(), s.max_len), ("cdr-lane", 0));
    }

    #[test]
    fn a_non_integer_bound_is_refused_naming_its_variable() {
        let e = RabbitMqCdrSettings::from_lookup(lookup(&[
            ("B2BUA_CDR_RABBITMQ_URL", "amqp://h/%2f"),
            ("B2BUA_CDR_RABBITMQ_MAX_LEN", "lots"),
        ]))
        .expect_err("a non-integer bound must refuse boot");
        assert!(e.contains("B2BUA_CDR_RABBITMQ_MAX_LEN"), "msg was: {e}");
    }

    /// The selected sink publishes: a write against an unparsable broker URI
    /// fails before any socket is opened and counts one dropped record, which
    /// the discarding default never does.
    #[tokio::test]
    async fn a_set_url_selects_a_sink_that_publishes_each_record() {
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
}
