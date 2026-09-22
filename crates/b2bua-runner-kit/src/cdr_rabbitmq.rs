//! The RabbitMQ CDR sink a runner composes from env: `B2BUA_CDR_RABBITMQ_URL`
//! set → a [`CdrWriter`] publishing one JSON [`b2bua::cdr::CdrRecord`] per
//! terminated call; unset → no sink, and [`crate::RunnerBase::deps`] keeps its
//! discarding default.

use std::sync::Arc;

use b2bua::cdr::CdrWriter;
use b2bua::metrics::B2buaMetrics;

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
    pub fn from_lookup(_get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        Ok(None)
    }
}

/// The RabbitMQ sink the settings read through `get` select, recording into
/// `metrics`; `Ok(None)` when no URL is set.
pub fn rabbitmq_cdr_sink_from_lookup(
    get: impl Fn(&str) -> Option<String>,
    _metrics: &B2buaMetrics,
) -> Result<Option<Arc<dyn CdrWriter>>, String> {
    let _ = RabbitMqCdrSettings::from_lookup(get)?;
    Ok(None)
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
