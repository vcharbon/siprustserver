//! The RabbitMQ CDR writer through a panic of its own code on a live session
//! ([`super::fault`]), against the scripted broker of the kit's integration
//! tests: every record counts once, and a panic, which tells nothing of the
//! broker, moves neither the broker cursor nor the backoff.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use b2bua::cdr::{BufferedCdrWriter, CdrEncodeError};

use super::tests::a_call;
use super::*;

#[path = "../../tests/it/support/fake_broker.rs"]
#[allow(dead_code)]
mod fake_broker;

use fake_broker::{Counters, FakeBroker, Reply};

const MS: Duration = Duration::from_millis(1);

/// Encodes every call as a few bytes, the encode numbered `at` (from 0) as
/// `fault`.
struct FaultAt {
    at: usize,
    fault: &'static [u8],
    encoded: AtomicUsize,
}

impl FaultAt {
    fn new(at: usize, fault: &'static [u8]) -> Arc<Self> {
        Arc::new(Self { at, fault, encoded: AtomicUsize::new(0) })
    }
}

impl CdrEncoder for FaultAt {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        if self.encoded.fetch_add(1, Ordering::SeqCst) == self.at {
            return Ok(self.fault.to_vec());
        }
        Ok(b"cdr".to_vec())
    }
}

/// A writer over `brokers`, in order, its first attempt at the first, encoding
/// through `encoder`.
fn writer_over(
    brokers: &[&FakeBroker],
    encoder: Arc<dyn CdrEncoder>,
) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let settings = RabbitMqCdrSettings {
        brokers: brokers.iter().map(|b| b.url().parse().unwrap()).collect(),
        queue: "cdr".into(),
        declare: CdrQueueDeclare::Own { max_len: 100_000 },
        bounds: CdrDeliveryBounds {
            connect_timeout: 400 * MS,
            publish_timeout: 200 * MS,
            confirm_timeout: 600 * MS,
            backoff_min: 300 * MS,
            backoff_max: 1_200 * MS,
            ..CdrDeliveryBounds::default()
        },
        tls: amqp_connect::BrokerTls::PLAIN,
    };
    (RabbitMqCdrWriter::new(settings, encoder, metrics.clone()).starting_at(0), metrics)
}

fn counts(m: &B2buaMetrics) -> (u64, u64) {
    (m.cdr_written_total(), m.cdr_dropped_total())
}

/// Waits (up to 900 ms) until `(written, dropped)` equals `want`.
async fn settle(m: &B2buaMetrics, want: (u64, u64)) {
    let give_up = Instant::now() + 900 * MS;
    while counts(m) != want && Instant::now() < give_up {
        tokio::time::sleep(MS).await;
    }
    assert_eq!(counts(m), want, "(written, dropped)");
}

/// Waits (up to 900 ms) until the broker counted `n` on `pick`.
async fn counted(broker: &FakeBroker, pick: impl Fn(&Counters) -> &AtomicUsize, n: usize) {
    let give_up = Instant::now() + 900 * MS;
    while broker.count(&pick) < n && Instant::now() < give_up {
        tokio::time::sleep(MS).await;
    }
    assert_eq!(broker.count(pick), n);
}

fn accepted(broker: &FakeBroker) -> usize {
    broker.count(|c| &c.accepted)
}

/// Record 0 is published on the first of two brokers, its ack sent 150 ms
/// later, when record 1's write panics holding the link; record 2 follows at
/// once. The panic retires the session: record 0 keeps its ack, the retired
/// session's socket closes after it, and record 2 opens a fresh session on
/// the same broker with no backoff.
#[tokio::test]
async fn a_write_panic_holding_the_link_retires_the_session_and_its_publish_keeps_its_ack() {
    let first = FakeBroker::start(Reply::AckAfter(150 * MS)).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_over(&[&first, &second], FaultAt::new(1, fault::PANICS_HOLDING_THE_LINK));
    let buf = BufferedCdrWriter::spawn(Arc::new(w), 64, m.clone());
    for _ in 0..3 {
        buf.write(&a_call(), 2_000).await;
    }
    settle(&m, (2, 1)).await;
    assert_eq!(m.cdr_drops(), [(CdrDrop::Panic, 1)], "drops by reason");
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::WriterPanic), 1);
    assert_eq!((accepted(&first), accepted(&second)), (2, 0), "a fresh session, same broker");
    counted(&first, |c| &c.ended_by_client, 1).await;
    buf.close(Instant::now() + 500 * MS).await;
    assert_eq!(counts(&m), (2, 1), "the close counts nothing twice");
}

/// The confirm tracker panics on the first record published on the first of
/// two brokers, its confirm outstanding: the record counts dropped for the
/// panic and the session ends for it. The next record, sent at once, opens a
/// fresh session on the same broker with no backoff and is written.
#[tokio::test]
async fn a_tracker_panic_costs_its_record_and_the_next_opens_on_the_same_broker_at_once() {
    let first = FakeBroker::start(Reply::Ack).await;
    let second = FakeBroker::start(Reply::Ack).await;
    let (w, m) = writer_over(&[&first, &second], FaultAt::new(0, fault::PANICS_THE_TRACKER));
    w.write(&a_call(), 2_000).await;
    settle(&m, (0, 1)).await;
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::TrackerPanic), 1);
    w.write(&a_call(), 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!(m.cdr_drops(), [(CdrDrop::Panic, 1)], "drops by reason");
    assert_eq!((accepted(&first), accepted(&second)), (2, 0), "a fresh session, same broker");
    w.close(Instant::now() + 500 * MS).await;
    assert_eq!(counts(&m), (1, 1), "the close counts nothing twice");
}
