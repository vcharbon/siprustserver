//! The RabbitMQ CDR sink over a list of brokers, against scripted ones: one
//! connect attempt reaches a broker past a dead or silent one, the next
//! attempt starts past a broker that failed and stays on one that delivered,
//! and writers given different starts open on different brokers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{CdrEncodeError, CdrEncoder, CdrWriter};
use b2bua::metrics::B2buaMetrics;
use b2bua_runner_kit::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
};
use call::Call;
use tokio::time::Instant;

use crate::cdr_rabbitmq_delivery::a_call;
use crate::support::fake_broker::{Admit, FakeBroker, Reply};

const MS: Duration = Duration::from_millis(1);

/// Encodes every call as a few bytes.
struct Fixed;

impl CdrEncoder for Fixed {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(b"cdr".to_vec())
    }
}

/// Bounds short enough for a test, long enough for a loopback broker.
fn bounds() -> CdrDeliveryBounds {
    CdrDeliveryBounds {
        window: 8,
        connect_timeout: 400 * MS,
        publish_timeout: 200 * MS,
        confirm_timeout: 600 * MS,
        backoff_min: 300 * MS,
        backoff_max: 1_200 * MS,
    }
}

/// A writer over `addrs`, in order, its first attempt starting at `first`.
fn writer_over(
    addrs: &[SocketAddr],
    first: usize,
    bounds: CdrDeliveryBounds,
) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let settings = RabbitMqCdrSettings {
        brokers: addrs
            .iter()
            .map(|addr| format!("amqp://guest:guest@{addr}/%2f").parse().unwrap())
            .collect(),
        queue: "cdr".into(),
        declare: CdrQueueDeclare::Own { max_len: 100_000 },
        bounds,
        tls: amqp_connect::BrokerTls::PLAIN,
    };
    let writer = RabbitMqCdrWriter::new(settings, Arc::new(Fixed), metrics.clone());
    (writer.starting_at(first), metrics)
}

fn counts(m: &B2buaMetrics) -> (u64, u64) {
    (m.cdr_written_total(), m.cdr_dropped_total())
}

/// Waits (up to 2 s) until `(written, dropped)` equals `want`.
async fn settle(m: &B2buaMetrics, want: (u64, u64)) {
    let give_up = Instant::now() + Duration::from_secs(2);
    while counts(m) != want && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    assert_eq!(counts(m), want, "(written, dropped)");
}

fn accepted(broker: &FakeBroker) -> usize {
    broker.count(|c| &c.accepted)
}

/// A loopback address that refuses every connect.
fn refusing() -> SocketAddr {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    addr
}

/// A listener whose zero backlog one connection fills: the kernel drops every
/// further SYN, so a connect to it hangs until its own bound.
fn black_holed() -> (socket2::Socket, std::net::TcpStream, SocketAddr) {
    use socket2::{Domain, Socket, Type};
    let listener = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    listener.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
    listener.listen(0).unwrap();
    let addr = listener.local_addr().unwrap().as_socket().unwrap();
    let filler = std::net::TcpStream::connect(addr).unwrap();
    let blocked = std::net::TcpStream::connect_timeout(&addr, 100 * MS);
    assert!(blocked.is_err(), "the full backlog must drop a further SYN");
    (listener, filler, addr)
}

#[tokio::test]
async fn a_record_is_written_through_the_next_broker_when_the_first_refuses() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_over(&[refusing(), broker.addr], 0, bounds());
    w.write(&a_call(), 2_000).await;
    settle(&m, (1, 0)).await;
    assert_eq!(accepted(&broker), 1);
}

/// The first broker never answers the SYN: it holds the attempt for its share
/// of the connect bound, and the second is connected within the rest.
#[tokio::test]
async fn a_record_is_written_through_the_next_broker_when_the_first_never_answers() {
    let (_held, _filler, silent) = black_holed();
    let broker = FakeBroker::start(Reply::Ack).await;
    let roomy = CdrDeliveryBounds { connect_timeout: 600 * MS, ..bounds() };
    let (w, m) = writer_over(&[silent, broker.addr], 0, roomy);
    let t0 = Instant::now();
    w.write(&a_call(), 2_000).await;
    let took = t0.elapsed();
    settle(&m, (1, 0)).await;
    assert_eq!(accepted(&broker), 1);
    assert!(took >= 290 * MS && took < 600 * MS, "the write took {took:?}: half the bound, then");
}

/// The first broker accepts TCP and stalls the AMQP handshake through the
/// whole attempt; once the backoff passed, the next attempt starts at the
/// second broker, though the first serves again.
#[tokio::test]
async fn the_attempt_after_a_failed_one_starts_at_the_next_broker() {
    let first = FakeBroker::start(Reply::Ack).await;
    let second = FakeBroker::start(Reply::Ack).await;
    first.admit(Admit::StallHandshake);
    let (w, m) = writer_over(&[first.addr, second.addr], 0, bounds());
    let call = a_call();
    w.write(&call, 2_000).await;
    assert_eq!(counts(&m), (0, 1));
    assert_eq!((accepted(&first), accepted(&second)), (1, 0), "the stall held the attempt");
    first.admit(Admit::Serve);
    tokio::time::sleep(350 * MS).await;
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!((accepted(&first), accepted(&second)), (1, 1));
}

/// A session that opened and ended having delivered nothing (its publish
/// returned) counts as the broker failing: the next attempt starts past it.
#[tokio::test]
async fn the_attempt_after_a_session_that_delivered_nothing_starts_at_the_next_broker() {
    let first = FakeBroker::start(Reply::Return).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_over(&[first.addr, second.addr], 0, bounds());
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, (0, 1)).await;
    first.reply(Reply::Ack);
    tokio::time::sleep(350 * MS).await;
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!((accepted(&first), accepted(&second)), (1, 1));
}

/// A session that delivered, then lost its connection: the next record
/// reconnects to the same broker at once.
#[tokio::test]
async fn the_broker_whose_session_delivered_is_tried_first_again() {
    let first = FakeBroker::start(Reply::Ack).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_over(&[first.addr, second.addr], 0, bounds());
    let call = a_call();
    w.write(&call, 2_000).await;
    settle(&m, (1, 0)).await;
    first.reply(Reply::Hangup);
    w.write(&call, 2_000).await;
    settle(&m, (1, 1)).await;
    first.reply(Reply::Ack);
    w.write(&call, 2_000).await;
    settle(&m, (2, 1)).await;
    assert_eq!((accepted(&first), accepted(&second)), (2, 0));
}

#[tokio::test]
async fn writers_with_different_starts_open_on_different_brokers() {
    let first = FakeBroker::start(Reply::Ack).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let brokers = [first.addr, second.addr];
    let (one, m1) = writer_over(&brokers, 0, bounds());
    let (other, m2) = writer_over(&brokers, 1, bounds());
    let call = a_call();
    one.write(&call, 2_000).await;
    other.write(&call, 2_000).await;
    settle(&m1, (1, 0)).await;
    settle(&m2, (1, 0)).await;
    assert_eq!((accepted(&first), accepted(&second)), (1, 1));
}
