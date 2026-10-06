//! The RabbitMQ CDR sink against a scripted broker: a record counts written
//! only on the broker's ack, every other end counts dropped, each record ends
//! in exactly one of the two, and no broker fault holds the writer past its
//! bounds.

use crate::support;
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
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use support::fake_broker::{Admit, FakeBroker, Reply};
use tokio::time::Instant;

const MS: Duration = Duration::from_millis(1);

fn a_call() -> Call {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-sink\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: sink-delivery@10.0.0.9\r\n\
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

/// Encodes every call as `size` bytes.
struct Sized(usize);

impl CdrEncoder for Sized {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(vec![b'x'; self.0])
    }
}

/// Bounds short enough for a test, long enough for a loopback broker.
fn bounds() -> CdrDeliveryBounds {
    CdrDeliveryBounds {
        window: 64,
        connect_timeout: 400 * MS,
        publish_timeout: 200 * MS,
        confirm_timeout: 600 * MS,
        backoff_min: 300 * MS,
        backoff_max: 1_200 * MS,
    }
}

fn writer_to(
    url: String,
    bounds: CdrDeliveryBounds,
    size: usize,
) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let settings = RabbitMqCdrSettings {
        url,
        queue: "cdr".into(),
        declare: CdrQueueDeclare::Own { max_len: 100_000 },
        bounds,
    };
    (RabbitMqCdrWriter::new(settings, Arc::new(Sized(size)), metrics.clone()), metrics)
}

fn counts(m: &B2buaMetrics) -> (u64, u64) {
    (m.cdr_written_total(), m.cdr_dropped_total())
}

/// Waits (up to 5 s) until `(written, dropped)` equals `want`.
async fn settle(m: &B2buaMetrics, want: (u64, u64)) {
    let give_up = Instant::now() + Duration::from_secs(5);
    while counts(m) != want && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    assert_eq!(counts(m), want, "(written, dropped)");
}

/// Waits (up to 2 s) until the broker received `n` publishes, so a reply
/// switched next applies to the following one only.
async fn published(broker: &FakeBroker, n: usize) {
    let give_up = Instant::now() + Duration::from_secs(2);
    while broker.count(|c| &c.published) < n && Instant::now() < give_up {
        tokio::time::sleep(MS).await;
    }
    assert_eq!(broker.count(|c| &c.published), n);
}

/// Writes one record, asserting the writer returned within `bound`.
async fn write_within(w: &RabbitMqCdrWriter, call: &Call, bound: Duration) {
    let t0 = Instant::now();
    w.write(call, 2_000).await;
    let took = t0.elapsed();
    assert!(took <= bound, "the write held the drainer {took:?}, past {bound:?}");
}

#[tokio::test]
async fn an_acked_publish_counts_one_written_record() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    for _ in 0..20 {
        w.write(&call, 2_000).await;
    }
    settle(&m, (20, 0)).await;
    assert_eq!(broker.count(|c| &c.accepted), 1, "one connection carries every record");
    assert_eq!(broker.count(|c| &c.published), 20);
}

#[tokio::test]
#[ignore = "slow lane: real clock >= 1 s"]
async fn a_publish_the_broker_never_confirms_is_not_written_and_drops_at_the_confirm_bound() {
    let broker = FakeBroker::start(Reply::Silent).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    write_within(&w, &call, 450 * MS).await;
    tokio::time::sleep(300 * MS).await;
    assert_eq!(counts(&m), (0, 0), "handed to the connection is not written");
    settle(&m, (0, 1)).await;
    // The connection that lost a confirm is shut down: the broker sees it end.
    let give_up = Instant::now() + Duration::from_secs(2);
    while broker.count(|c| &c.ended_by_client) < 1 && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    assert_eq!(broker.count(|c| &c.ended_by_client), 1);
    // It had delivered nothing: the next record waits out the backoff.
    broker.reply(Reply::Ack);
    w.write(&call, 2_000).await;
    settle(&m, (0, 2)).await;
    assert_eq!(broker.count(|c| &c.accepted), 1, "no connect inside the backoff");
    tokio::time::sleep(350 * MS).await;
    w.write(&call, 2_000).await;
    settle(&m, (1, 2)).await;
    assert_eq!(broker.count(|c| &c.accepted), 2);
}

#[tokio::test]
async fn a_nacked_publish_counts_dropped_and_the_connection_carries_on() {
    let broker = FakeBroker::start(Reply::Nack).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, (0, 1)).await;
    broker.reply(Reply::Ack);
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!(broker.count(|c| &c.accepted), 1, "a nack does not end the connection");
}

#[tokio::test]
async fn a_returned_publish_counts_dropped_and_the_next_record_declares_the_queue_again() {
    let broker = FakeBroker::start(Reply::Return).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, (0, 1)).await;
    assert_eq!(broker.count(|c| &c.declared), 1);
    broker.reply(Reply::Ack);
    // The returning connection delivered nothing: past the backoff it reconnects.
    tokio::time::sleep(350 * MS).await;
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!(broker.count(|c| &c.accepted), 2, "a returned publish ends the connection");
    assert_eq!(broker.count(|c| &c.declared), 2, "the reconnect declares the queue again");
}

#[tokio::test]
async fn a_connection_lost_with_confirms_outstanding_drops_each_of_them_without_waiting() {
    let broker = FakeBroker::start(Reply::Silent).await;
    let slow_confirm = CdrDeliveryBounds { confirm_timeout: Duration::from_secs(30), ..bounds() };
    let (w, m) = writer_to(broker.url(), slow_confirm, 64);
    let call = a_call();
    for _ in 0..3 {
        w.write(&call, 2_000).await;
    }
    broker.reply(Reply::Hangup);
    w.write(&call, 2_000).await;
    // Far inside the 30 s confirm bound: the lost connection fails them all.
    settle(&m, (0, 4)).await;
}

#[tokio::test]
async fn a_connection_that_delivered_reconnects_at_once_when_it_ends() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, (1, 0)).await;
    broker.reply(Reply::Hangup);
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    broker.reply(Reply::Ack);
    w.write(&call, 2_000).await;
    settle(&m, (2, 1)).await;
    assert_eq!(broker.count(|c| &c.accepted), 2);
}

#[tokio::test]
async fn a_stalled_handshake_holds_the_writer_no_longer_than_the_connect_bound_and_is_shut_down() {
    let broker = FakeBroker::start(Reply::Ack).await;
    broker.admit(Admit::StallHandshake);
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    write_within(&w, &call, 500 * MS).await;
    assert_eq!(counts(&m), (0, 1));
    // The abandoned connection's socket is shut down, not left to its IO loop.
    let give_up = Instant::now() + Duration::from_secs(2);
    while broker.count(|c| &c.ended_by_client) < 1 && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    assert_eq!(broker.count(|c| &c.ended_by_client), 1, "the stalled connection must be ended");
    // Inside the backoff every record drops at once, with no connect attempt.
    for _ in 0..50 {
        write_within(&w, &call, 20 * MS).await;
    }
    assert_eq!(counts(&m), (0, 51));
    assert_eq!(broker.count(|c| &c.accepted), 1);
    // Past it the broker is healthy again and the next record is delivered.
    broker.admit(Admit::Serve);
    tokio::time::sleep(350 * MS).await;
    w.write(&call, 2_000).await;
    settle(&m, (1, 51)).await;
}

#[tokio::test]
async fn a_refused_connect_drops_the_record_and_backs_off() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let (w, m) = writer_to(format!("amqp://guest:guest@{addr}/%2f"), bounds(), 64);
    let call = a_call();
    for _ in 0..10 {
        write_within(&w, &call, 450 * MS).await;
    }
    assert_eq!(counts(&m), (0, 10));
}

#[tokio::test]
#[ignore = "slow lane: real clock >= 1 s"]
async fn consecutive_failed_connects_double_the_backoff() {
    let broker = FakeBroker::start(Reply::Ack).await;
    broker.admit(Admit::StallHandshake);
    let quick = CdrDeliveryBounds { connect_timeout: 50 * MS, ..bounds() };
    let (w, _m) = writer_to(broker.url(), quick, 64);
    let call = a_call();
    // Attempts at 0, then after 300 ms and 600 ms waits: three connects by
    // ~1.1 s, while a constant 300 ms backoff would have made four.
    let t0 = Instant::now();
    while t0.elapsed() < 1_150 * MS {
        w.write(&call, 2_000).await;
        tokio::time::sleep(10 * MS).await;
    }
    assert_eq!(broker.count(|c| &c.accepted), 3);
}

#[tokio::test]
async fn a_flow_controlled_connection_drops_at_the_publish_bound() {
    let broker = FakeBroker::start(Reply::Ack).await;
    broker.admit(Admit::ServeBlocked);
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    // Connect, then the blocked publish: past the connect, the publish bound.
    write_within(&w, &call, 450 * MS).await;
    assert_eq!(counts(&m), (0, 1));
    assert_eq!(broker.count(|c| &c.published), 0, "a blocked connection sends no publish");
    // The connection delivered nothing: the backoff drops the next at once.
    for _ in 0..3 {
        write_within(&w, &call, 20 * MS).await;
    }
    assert_eq!(counts(&m), (0, 4));
    assert_eq!(broker.count(|c| &c.accepted), 1);
}

#[tokio::test]
async fn a_broker_that_stops_reading_holds_the_writer_no_longer_than_the_publish_bound() {
    let broker = FakeBroker::start(Reply::StopReading).await;
    let (w, m) = writer_to(broker.url(), bounds(), 1 << 20);
    let call = a_call();
    // 1 MiB records: the socket buffers fill within a few, then the hand-off
    // stalls and the publish bound ends the connection.
    for _ in 0..40 {
        write_within(&w, &call, 450 * MS).await;
    }
    let (written, dropped) = counts(&m);
    assert_eq!(written, 0);
    assert!(dropped > 0, "records past the stall must drop");
    settle(&m, (0, 40)).await;
}

#[tokio::test]
#[ignore = "slow lane: real clock >= 1 s"]
async fn a_full_window_drops_the_record_at_the_publish_bound_and_keeps_the_connection() {
    let broker = FakeBroker::start(Reply::Silent).await;
    let tiny = CdrDeliveryBounds { window: 2, confirm_timeout: Duration::from_secs(2), ..bounds() };
    let (w, m) = writer_to(broker.url(), tiny, 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    w.write(&call, 2_000).await;
    let t0 = Instant::now();
    w.write(&call, 2_000).await;
    let took = t0.elapsed();
    assert!(took >= 190 * MS && took <= 300 * MS, "waited {took:?} for a slot");
    assert_eq!(counts(&m), (0, 1), "the third record found the window full");
    assert_eq!(broker.count(|c| &c.published), 2, "no publish beyond the window");
    // The two unconfirmed publishes end at the confirm bound.
    settle(&m, (0, 3)).await;
    assert_eq!(broker.count(|c| &c.accepted), 1);
}

/// With the broker answering each publish after 5 ms, serial confirms would
/// cap the writer at 200 records/s; the window pipelines them.
#[tokio::test]
async fn the_window_pipelines_confirms_past_one_per_round_trip() {
    let broker = FakeBroker::start(Reply::AckAfter(5 * MS)).await;
    let (w, m) = writer_to(broker.url(), CdrDeliveryBounds { window: 256, ..bounds() }, 256);
    let call = a_call();
    let t0 = Instant::now();
    for _ in 0..1_000 {
        w.write(&call, 2_000).await;
    }
    settle(&m, (1_000, 0)).await;
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(2), "1000 records took {took:?}: >= 500/s expected");
}

/// Every record ends in exactly one of written or dropped, whatever the broker
/// answers and however the connection ends.
#[tokio::test]
async fn every_record_ends_written_or_dropped_exactly_once() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let quick = CdrDeliveryBounds { backoff_min: 20 * MS, backoff_max: 40 * MS, ..bounds() };
    let (w, m) = writer_to(broker.url(), quick, 64);
    let call = a_call();
    let script = [
        Reply::Ack,
        Reply::Nack,
        Reply::Ack,
        Reply::Return,
        Reply::Ack,
        Reply::Silent,
        Reply::Hangup,
        Reply::Ack,
    ];
    let mut n = 0u64;
    for round in 0..5 {
        for r in script {
            broker.reply(r);
            w.write(&call, 2_000).await;
            n += 1;
        }
        tokio::time::sleep(if round % 2 == 0 { 50 * MS } else { Duration::ZERO }).await;
    }
    let give_up = Instant::now() + Duration::from_secs(5);
    while m.cdr_written_total() + m.cdr_dropped_total() < n && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    tokio::time::sleep(700 * MS).await;
    let (written, dropped) = counts(&m);
    assert_eq!(written + dropped, n, "written {written} + dropped {dropped} != {n} records");
    assert!(written > 0 && dropped > 0);
}

#[tokio::test]
async fn a_multiple_ack_counts_every_publish_it_covers_once() {
    let broker = FakeBroker::start(Reply::AckMultipleEvery(10)).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    for _ in 0..100 {
        w.write(&call, 2_000).await;
    }
    settle(&m, (100, 0)).await;
    tokio::time::sleep(100 * MS).await;
    assert_eq!(counts(&m), (100, 0), "no record counted twice");
}

/// Publish 1 is never confirmed; publish 2 is acked at once. Publish 1's
/// confirm bound ends the session with publish 2 still queued behind it in
/// the tracker: its ack, already received, still counts it written.
#[tokio::test]
async fn a_publish_acked_before_its_session_ends_is_counted_written() {
    let broker = FakeBroker::start(Reply::Silent).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    published(&broker, 1).await;
    broker.reply(Reply::Ack);
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
}

/// With one window slot taken by a publish the broker never confirms, the
/// next record waits for a slot; the session ending at the (shorter) confirm
/// bound releases it at once, before the publish bound.
#[tokio::test]
async fn a_record_waiting_for_a_slot_returns_when_the_session_ends() {
    let broker = FakeBroker::start(Reply::Silent).await;
    let b = CdrDeliveryBounds {
        window: 1,
        publish_timeout: 2_000 * MS,
        confirm_timeout: 300 * MS,
        ..bounds()
    };
    let (w, m) = writer_to(broker.url(), b, 64);
    let call = a_call();
    w.write(&call, 2_000).await;
    let t0 = Instant::now();
    w.write(&call, 2_000).await;
    let took = t0.elapsed();
    assert!(took < 1_000 * MS, "the waiter took {took:?}: the session end must release it");
    settle(&m, (0, 2)).await;
    assert_eq!(broker.count(|c| &c.published), 1, "nothing is published on an ended session");
}
