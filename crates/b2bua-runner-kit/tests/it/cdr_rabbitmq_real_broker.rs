//! The RabbitMQ CDR sink against a real broker, run by hand:
//!
//! ```sh
//! docker run -d --name cdr-sink-broker -p 127.0.0.1:5673:5672 rabbitmq:3.13-management
//! CDR_SINK_TEST_AMQP_URL=amqp://guest:guest@127.0.0.1:5673/%2f \
//! CDR_SINK_TEST_FREEZE='docker pause cdr-sink-broker' \
//! CDR_SINK_TEST_THAW='docker unpause cdr-sink-broker' \
//!   cargo test -p b2bua-runner-kit --test it cdr_rabbitmq_real_broker:: -- --ignored --test-threads=1
//! ```
//!
//! Each test skips (passes) when `CDR_SINK_TEST_AMQP_URL` is unset.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{CdrEncodeError, CdrEncoder, CdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::metrics::B2buaMetrics;
use b2bua_runner_kit::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
};
use call::Call;
use lapin::options::{QueueDeleteOptions, QueuePurgeOptions};
use lapin::{Connection, ConnectionProperties};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use tokio::time::Instant;

fn a_call() -> Call {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-real\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: sink-real@10.0.0.9\r\n\
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
        &sip_txn::IdGen::seeded(1),
        1_000,
    )
}

struct Fixed;

impl CdrEncoder for Fixed {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(vec![b'x'; 512])
    }
}

fn url() -> Option<String> {
    std::env::var("CDR_SINK_TEST_AMQP_URL").ok().filter(|u| !u.is_empty())
}

fn writer(url: String, queue: &str, declare: CdrQueueDeclare) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let settings = RabbitMqCdrSettings {
        url,
        queue: queue.into(),
        declare,
        bounds: CdrDeliveryBounds::default(),
    };
    (RabbitMqCdrWriter::new(settings, Arc::new(Fixed), metrics.clone()), metrics)
}

async fn admin(url: &str) -> lapin::Channel {
    let props = ConnectionProperties::default()
        .with_executor(tokio_executor_trait::Tokio::current())
        .with_reactor(tokio_reactor_trait::Tokio);
    let conn = Connection::connect(url, props).await.expect("admin connect");
    let chan = conn.create_channel().await.expect("admin channel");
    // The connection must outlive the channel: leak it for the test's life.
    std::mem::forget(conn);
    chan
}

async fn settle(m: &B2buaMetrics, n: u64, within: Duration) {
    let give_up = Instant::now() + within;
    while m.cdr_written_total() + m.cdr_dropped_total() < n && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn sh(cmd: &str) {
    let ok = std::process::Command::new("sh").arg("-c").arg(cmd).status().expect("sh").success();
    assert!(ok, "`{cmd}` failed");
}

#[tokio::test]
#[ignore = "needs a RabbitMQ broker: CDR_SINK_TEST_AMQP_URL"]
async fn every_record_the_broker_acks_is_in_the_queue() {
    let Some(url) = url() else { return };
    let chan = admin(&url).await;
    let queue = "cdr-sink-acked";
    let (w, m) = writer(url, queue, CdrQueueDeclare::Own { max_len: 0 });
    let call = a_call();
    let t0 = Instant::now();
    for _ in 0..5_000 {
        w.write(&call, 2_000).await;
    }
    settle(&m, 5_000, Duration::from_secs(10)).await;
    let took = t0.elapsed();
    let held = chan
        .queue_declare(
            queue,
            lapin::options::QueueDeclareOptions { passive: true, ..Default::default() },
            Default::default(),
        )
        .await
        .expect("passive declare")
        .message_count();
    chan.queue_delete(queue, QueueDeleteOptions::default()).await.expect("cleanup");
    eprintln!("5000 records acked in {took:?}; queue held {held}");
    assert_eq!((m.cdr_written_total(), m.cdr_dropped_total()), (5_000, 0));
    assert_eq!(held, 5_000, "every acked record is in the queue");
    assert!(took < Duration::from_secs(10), "{took:?} for 5000 records: >= 500/s expected");
}

#[tokio::test]
#[ignore = "needs a RabbitMQ broker: CDR_SINK_TEST_AMQP_URL"]
async fn a_publish_to_a_deleted_queue_is_returned_and_counted_dropped() {
    let Some(url) = url() else { return };
    let chan = admin(&url).await;
    let queue = "cdr-sink-deleted";
    chan.queue_declare(queue, Default::default(), Default::default()).await.expect("declare");
    let (w, m) = writer(url, queue, CdrQueueDeclare::Existing);
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, 1, Duration::from_secs(5)).await;
    assert_eq!((m.cdr_written_total(), m.cdr_dropped_total()), (1, 0));
    chan.queue_delete(queue, QueueDeleteOptions::default()).await.expect("delete");
    w.write(&call, 2_000).await;
    settle(&m, 2, Duration::from_secs(5)).await;
    assert_eq!((m.cdr_written_total(), m.cdr_dropped_total()), (1, 1), "returned, not written");
    // The broker-held queue is gone: the reconnect's passive declare fails.
    tokio::time::sleep(Duration::from_millis(600)).await;
    w.write(&call, 2_000).await;
    assert_eq!((m.cdr_written_total(), m.cdr_dropped_total()), (1, 2));
}

#[tokio::test]
#[ignore = "needs a RabbitMQ broker and CDR_SINK_TEST_FREEZE / _THAW commands"]
async fn a_frozen_broker_turns_into_bounded_writes_and_counted_drops() {
    let Some(url) = url() else { return };
    let (Ok(freeze), Ok(thaw)) =
        (std::env::var("CDR_SINK_TEST_FREEZE"), std::env::var("CDR_SINK_TEST_THAW"))
    else {
        return;
    };
    let chan = admin(&url).await;
    let queue = "cdr-sink-frozen";
    let (w, m) = writer(url, queue, CdrQueueDeclare::Own { max_len: 0 });
    let call = a_call();
    for _ in 0..100 {
        w.write(&call, 2_000).await;
    }
    settle(&m, 100, Duration::from_secs(5)).await;
    assert_eq!((m.cdr_written_total(), m.cdr_dropped_total()), (100, 0));

    sh(&freeze);
    // 200 records/s for 10 s against the frozen broker.
    let mut slowest = Duration::ZERO;
    let frozen_at = Instant::now();
    for _ in 0..2_000 {
        let t0 = Instant::now();
        w.write(&call, 2_000).await;
        slowest = slowest.max(t0.elapsed());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let frozen_for = frozen_at.elapsed();
    sh(&thaw);
    let d = CdrDeliveryBounds::default();
    eprintln!(
        "frozen {frozen_for:?}: slowest write {slowest:?}; written {} dropped {}",
        m.cdr_written_total(),
        m.cdr_dropped_total()
    );
    assert!(slowest <= d.connect_timeout + Duration::from_millis(200), "slowest {slowest:?}");
    // After the thaw the sink delivers again, and every record is accounted.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let before = m.cdr_written_total();
    for _ in 0..100 {
        w.write(&call, 2_000).await;
    }
    settle(&m, 2_200, Duration::from_secs(10)).await;
    eprintln!("after thaw: written {} dropped {}", m.cdr_written_total(), m.cdr_dropped_total());
    assert!(m.cdr_written_total() >= before + 100, "the sink delivers again after the thaw");
    assert_eq!(m.cdr_written_total() + m.cdr_dropped_total(), 2_200);
    chan.queue_purge(queue, QueuePurgeOptions::default()).await.ok();
    chan.queue_delete(queue, QueueDeleteOptions::default()).await.ok();
}
