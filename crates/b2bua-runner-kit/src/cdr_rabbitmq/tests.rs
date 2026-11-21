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
    build_initial_call(
        &req,
        SocketAddr::from(([10, 0, 0, 9], 5060)),
        &cfg,
        &mut sip_ids::TurnIds::fixed(1),
        1_000,
    )
}

fn settings(declare: CdrQueueDeclare) -> RabbitMqCdrSettings {
    RabbitMqCdrSettings {
        brokers: vec!["amqp://h/%2f".parse().unwrap()],
        queue: "cdr".into(),
        declare,
        bounds: CdrDeliveryBounds::default(),
        tls: amqp_connect::BrokerTls::PLAIN,
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
    let w = writer(OWN_DEFAULT, Arc::new(JsonRecordEncoder));
    let call = a_call();
    for terminated_at in [2_000, 500] {
        let expected = serde_json::to_vec(&build_record(&call, terminated_at)).unwrap();
        assert_eq!(w.payload(&call, terminated_at), Some(expected), "t={terminated_at}");
    }
}

/// Remembers every end it is told.
#[derive(Default)]
struct Ends(std::sync::Mutex<Vec<b2bua::cdr::CdrDelivery>>);

impl CdrDeliveryObserver for Ends {
    fn ended(&self, delivery: b2bua::cdr::CdrDelivery, _took: std::time::Duration) {
        self.0.lock().unwrap().push(delivery);
    }
}

#[tokio::test]
async fn an_encode_refusal_counts_one_dropped_record_and_tells_the_observer() {
    let metrics = B2buaMetrics::new();
    let enc = Arc::new(RefusingEncoder::default());
    let ends = Arc::new(Ends::default());
    let w = RabbitMqCdrWriter::new(settings(OWN_DEFAULT), enc.clone(), metrics.clone())
        .observed_by(ends.clone());
    w.write(&a_call(), 2_000).await;
    assert_eq!(enc.calls.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.cdr_written_total(), 0);
    assert_eq!(metrics.cdr_drops(), [(CdrDrop::Encode, 1)]);
    assert_eq!(*ends.0.lock().unwrap(), vec![b2bua::cdr::CdrDelivery::Dropped]);
}

/// The native record of [`a_call`], created at 1000 and terminated at 2000,
/// as the RabbitMQ sink publishes it.
const GOLDEN: &str = concat!(
    r#"{"call_ref":"w0|sink-probe@10.0.0.9|alicetag","#,
    r#""incarnation":"w0|sink-probe@10.0.0.9|alicetag#94dmne6v","created_at":1000,"terminated_at":2000,"#,
    r#""a_leg":{"call_id":"sink-probe@10.0.0.9","from_tag":"alicetag","state":"trying"},"#,
    r#""b_legs":[],"events":[{"type":"invite_received","timestamp":1000,"leg_id":"a","#,
    r#""status_code":null,"reason":null,"decision_ordinal":0}],"decision_log":[],"#,
    r#""termination":null}"#
);

/// The same call discharged at 500, before its creation: clamped and flagged.
const GOLDEN_SKEWED: &str = concat!(
    r#"{"call_ref":"w0|sink-probe@10.0.0.9|alicetag","#,
    r#""incarnation":"w0|sink-probe@10.0.0.9|alicetag#94dmne6v","created_at":1000,"terminated_at":1000,"#,
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

/// A broker address no one listens on: a connect is refused at once.
const REFUSING_BROKER: &str = "amqp://127.0.0.1:9/%2f";

/// The selected sink publishes: a write against a broker port that refuses
/// the connect counts one dropped record, which the discarding default
/// never does.
#[tokio::test]
async fn a_set_url_selects_the_broker_sink_not_the_discarding_default() {
    let metrics = B2buaMetrics::new();
    let sink = rabbitmq_cdr_writer_from_lookup(
        lookup(&[("B2BUA_CDR_RABBITMQ_URL", REFUSING_BROKER)]),
        &metrics,
    )
    .expect("ok")
    .expect("a set URL must select the RabbitMQ sink");
    sink.write(&a_call(), 2_000).await;
    let refused = [(CdrDrop::NoBroker, 1)];
    assert_eq!(metrics.cdr_drops(), refused, "the publish attempt must be counted");
    assert_eq!(metrics.cdr_written_total(), 0);
    assert_eq!(metrics.cdr_broker_connects_of(BrokerConnect::Failed), 1);
    assert_eq!(metrics.cdr_broker_connected(), 0);
}

/// The sink selected with an encoder encodes through it: a refusing encoder
/// is called once per write and each refusal is one dropped record.
#[tokio::test]
async fn a_sink_selected_with_an_encoder_encodes_through_it() {
    let metrics = B2buaMetrics::new();
    let enc = Arc::new(RefusingEncoder::default());
    let sink = rabbitmq_cdr_writer_from_lookup_with_encoder(
        lookup(&[("B2BUA_CDR_RABBITMQ_URL", REFUSING_BROKER)]),
        enc.clone(),
        &metrics,
    )
    .expect("ok")
    .expect("a set URL must select the RabbitMQ sink");
    sink.write(&a_call(), 2_000).await;
    sink.write(&a_call(), 3_000).await;
    assert_eq!(enc.calls.load(Ordering::SeqCst), 2);
    assert_eq!(metrics.cdr_written_total(), 0);
    assert_eq!(metrics.cdr_drops(), [(CdrDrop::Encode, 2)], "refused before any connect");
    assert_eq!(metrics.cdr_broker_connects_of(BrokerConnect::Failed), 0);
}
