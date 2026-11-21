//! The RabbitMQ CDR sink's link, as `/metrics` reads it: the live session
//! gauge, each connect attempt by outcome and each session's end by reason.

use std::time::Duration;

use b2bua::cdr::{BrokerConnect, BrokerSessionEnd, CdrWriter};
use b2bua::metrics::B2buaMetrics;
use tokio::time::Instant;

use crate::cdr_rabbitmq_delivery::{a_call, bounds, settle, writer_to};
use crate::support::fake_broker::{FakeBroker, Reply};

const MS: Duration = Duration::from_millis(1);

/// Waits (up to 900 ms) until the link gauge reads `want`.
async fn connected(m: &B2buaMetrics, want: u64) {
    let give_up = Instant::now() + 900 * MS;
    while m.cdr_broker_connected() != want && Instant::now() < give_up {
        tokio::time::sleep(MS).await;
    }
    assert_eq!(m.cdr_broker_connected(), want, "b2bua_cdr_broker_connected");
}

/// The gauge reads 0 before the first record, 1 once its connect opened a
/// session, 0 once the broker's end of the idle socket ended it, 1 again
/// after the next record's connect, and 0 once the writer closed: each
/// connect and each end counted under its outcome and reason.
#[tokio::test]
async fn the_link_gauge_follows_each_session_and_its_connects_and_ends_are_counted() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_to(broker.url(), bounds(), 64);
    let call = a_call();
    assert_eq!(m.cdr_broker_connected(), 0, "no session before the first record");
    w.write(&call, 2_000).await;
    settle(&m, (1, 0)).await;
    assert_eq!(m.cdr_broker_connected(), 1);
    assert_eq!(m.cdr_broker_connects_of(BrokerConnect::Ok), 1);
    broker.hang_up_idle();
    connected(&m, 0).await;
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::ConnectionLost), 1);
    w.write(&call, 2_000).await;
    settle(&m, (2, 0)).await;
    assert_eq!(m.cdr_broker_connected(), 1);
    assert_eq!(m.cdr_broker_connects_of(BrokerConnect::Ok), 2);
    w.close(Instant::now() + 500 * MS).await;
    assert_eq!(m.cdr_broker_connected(), 0, "the close ends the session");
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::Closed), 1);
    let ends: u64 =
        BrokerSessionEnd::ALL.iter().map(|&why| m.cdr_broker_session_ends_of(why)).sum();
    assert_eq!(ends, 2, "one end per session");
    let failed = [BrokerConnect::Failed, BrokerConnect::TimedOut];
    assert_eq!(failed.map(|o| m.cdr_broker_connects_of(o)), [0, 0]);
}
