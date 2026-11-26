//! The RabbitMQ CDR sink over a list of brokers, against scripted ones: one
//! connect attempt reaches a broker past a dead, silent or stalling one, the
//! next attempt starts past a session that failed and stays on one that
//! delivered, and writers open on different brokers.

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
use crate::support::fake_broker::{Admit, FakeBroker, Refusing, Reply, ROOMY, SETTLE_WITHIN};

const MS: Duration = Duration::from_millis(1);

/// A connect bound whose tenth is a broker's share in a list of ten: the
/// nine tenths left are far past any delay a loaded box adds.
const TEN_SHARES: Duration = Duration::from_secs(3);

/// Encodes every call as a few bytes.
struct Fixed;

impl CdrEncoder for Fixed {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        Ok(b"cdr".to_vec())
    }
}

/// Connect, publish and confirm bounds a loopback broker meets on a loaded
/// box, and a backoff short enough for a test to wait past.
fn bounds() -> CdrDeliveryBounds {
    CdrDeliveryBounds {
        window: 8,
        connect_timeout: ROOMY,
        publish_timeout: ROOMY,
        confirm_timeout: ROOMY,
        backoff_min: 300 * MS,
        backoff_max: 1_200 * MS,
    }
}

/// A writer over `addrs`, in order, its first attempt starting at the broker
/// it draws.
fn drawing_writer_over(
    addrs: &[SocketAddr],
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
    (RabbitMqCdrWriter::new(settings, Arc::new(Fixed), metrics.clone()), metrics)
}

/// A writer over `addrs`, in order, its first attempt starting at `first`.
fn writer_over(
    addrs: &[SocketAddr],
    first: usize,
    bounds: CdrDeliveryBounds,
) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let (writer, metrics) = drawing_writer_over(addrs, bounds);
    (writer.starting_at(first), metrics)
}

fn counts(m: &B2buaMetrics) -> (u64, u64) {
    (m.cdr_written_total(), m.cdr_dropped_total())
}

/// Waits (up to [`SETTLE_WITHIN`]) until `(written, dropped)` equals `want`.
async fn settle(m: &B2buaMetrics, want: (u64, u64)) {
    let give_up = Instant::now() + SETTLE_WITHIN;
    while counts(m) != want && Instant::now() < give_up {
        tokio::time::sleep(5 * MS).await;
    }
    assert_eq!(counts(m), want, "(written, dropped)");
}

fn accepted(broker: &FakeBroker) -> usize {
    broker.count(|c| &c.accepted)
}

/// Waits (up to [`SETTLE_WITHIN`]) until `broker` accepted `n` connections: a
/// connection the client gave up on is accepted once the broker's task runs.
async fn accepted_by(broker: &FakeBroker, n: usize) {
    let give_up = Instant::now() + SETTLE_WITHIN;
    while accepted(broker) < n && Instant::now() < give_up {
        tokio::time::sleep(MS).await;
    }
    assert_eq!(accepted(broker), n);
}

/// `first`, eight refusing brokers, then `last`: with a bound of
/// [`TEN_SHARES`], `first`'s share is a tenth of it, and `last` is tried
/// within the rest. The refusing ports are held while the first value lives.
fn first_and_last_of_ten(first: SocketAddr, last: SocketAddr) -> (Vec<Refusing>, Vec<SocketAddr>) {
    let refusing: Vec<Refusing> = (0..8).map(|_| Refusing::bind()).collect();
    let mut listed = vec![first];
    listed.extend(refusing.iter().map(|r| r.addr));
    listed.push(last);
    (refusing, listed)
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
    let refusing = Refusing::bind();
    let (w, m) = writer_over(&[refusing.addr, broker.addr], 0, bounds());
    w.write(&a_call(), 2_000).await;
    settle(&m, (1, 0)).await;
    assert_eq!(accepted(&broker), 1);
}

/// The first broker never answers the SYN: it holds the attempt for its share
/// of the connect bound, and the last is connected within the rest.
#[tokio::test]
async fn a_record_is_written_through_a_later_broker_when_the_first_never_answers() {
    let (_held, _filler, silent) = black_holed();
    let broker = FakeBroker::start(Reply::Ack).await;
    let ten = CdrDeliveryBounds { connect_timeout: TEN_SHARES, ..bounds() };
    let (_refusing, listed) = first_and_last_of_ten(silent, broker.addr);
    let (w, m) = writer_over(&listed, 0, ten);
    let t0 = Instant::now();
    w.write(&a_call(), 2_000).await;
    let took = t0.elapsed();
    settle(&m, (1, 0)).await;
    assert_eq!(accepted(&broker), 1);
    let share = TEN_SHARES / 10;
    assert!(took >= share - 10 * MS, "the write took {took:?}: under the first's share");
}

/// The first broker accepts TCP and stalls the AMQP handshake: it holds the
/// attempt for its share of the connect bound, and the last is connected
/// within the rest.
#[tokio::test]
async fn a_record_is_written_through_a_later_broker_when_the_first_stalls_its_handshake() {
    let stalled = FakeBroker::start(Reply::Ack).await;
    let healthy = FakeBroker::start(Reply::Ack).await;
    stalled.admit(Admit::StallHandshake);
    let ten = CdrDeliveryBounds { connect_timeout: TEN_SHARES, ..bounds() };
    let (_refusing, listed) = first_and_last_of_ten(stalled.addr, healthy.addr);
    let (w, m) = writer_over(&listed, 0, ten);
    let t0 = Instant::now();
    w.write(&a_call(), 2_000).await;
    let took = t0.elapsed();
    settle(&m, (1, 0)).await;
    accepted_by(&stalled, 1).await;
    assert_eq!(accepted(&healthy), 1);
    let share = TEN_SHARES / 10;
    assert!(took >= share - 10 * MS, "the write took {took:?}: under the first's share");
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

/// Writers given no start draw one each: 24 of them all opening on one of
/// two brokers is a fixed draw (a chance of 2 in 2^24 otherwise).
#[tokio::test]
async fn writers_given_no_start_open_on_both_brokers() {
    let first = FakeBroker::start(Reply::Ack).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let call = a_call();
    let mut writers = Vec::new();
    for _ in 0..24 {
        let (w, m) = drawing_writer_over(&[first.addr, second.addr], bounds());
        w.write(&call, 2_000).await;
        settle(&m, (1, 0)).await;
        writers.push(w);
    }
    let (one, other) = (accepted(&first), accepted(&second));
    assert_eq!(one + other, 24, "each writer opened one connection");
    assert!(one > 0 && other > 0, "every writer opened on one broker: ({one}, {other})");
}
