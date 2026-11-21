//! The RabbitMQ CDR sink through a panic outside its session (its encoder,
//! its observer): the session serves on, every record counts once, and the
//! observer is told each end.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::{
    BrokerSessionEnd, BufferedCdrWriter, CdrDelivery, CdrDeliveryObserver, CdrDrop, CdrEncodeError,
    CdrEncoder, CdrWriter,
};
use b2bua::metrics::B2buaMetrics;
use b2bua::supervise::{CDR_DRAINER, RESTARTS};
use b2bua_runner_kit::{
    CdrDeliveryBounds, CdrQueueDeclare, RabbitMqCdrSettings, RabbitMqCdrWriter,
};
use call::Call;
use tokio::time::Instant;

use crate::cdr_rabbitmq_delivery::a_call;
use crate::support::fake_broker::{Counters, FakeBroker, Reply};

const MS: Duration = Duration::from_millis(1);

/// Takes one from `left` when it is not zero.
fn take(left: &AtomicUsize) -> bool {
    left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
}

/// Encodes every call as a few bytes; the encode numbered `panics_at`
/// (from 0) panics.
struct PanicsAt {
    panics_at: usize,
    encoded: AtomicUsize,
}

impl CdrEncoder for PanicsAt {
    fn encode(&self, _call: &Call, _terminated_at: i64) -> Result<Vec<u8>, CdrEncodeError> {
        if self.encoded.fetch_add(1, Ordering::SeqCst) == self.panics_at {
            panic!("expected: the encoder panics");
        }
        Ok(b"cdr".to_vec())
    }
}

/// Remembers every end it is told; its next `panics` calls panic before
/// they remember.
struct Ends {
    panics: AtomicUsize,
    told: std::sync::Mutex<Vec<CdrDelivery>>,
}

impl CdrDeliveryObserver for Ends {
    fn ended(&self, delivery: CdrDelivery, _took: Duration) {
        if take(&self.panics) {
            panic!("expected: the observer panics");
        }
        self.told.lock().unwrap().push(delivery);
    }
}

fn writer_to(
    broker: &FakeBroker,
    encoder: Arc<dyn CdrEncoder>,
) -> (RabbitMqCdrWriter, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let settings = RabbitMqCdrSettings {
        brokers: vec![broker.url().parse().unwrap()],
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
    (RabbitMqCdrWriter::new(settings, encoder, metrics.clone()), metrics)
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

/// A record's encode panics while the record before it, published on a
/// healthy session, awaits an ack the broker sends 100 ms later, and the
/// record after it follows at once: the panic counts its own record dropped
/// once, and the records around it are written over the same connection.
#[tokio::test]
async fn a_write_panic_outside_the_session_spares_the_records_around_it() {
    let broker = FakeBroker::start(Reply::AckAfter(100 * MS)).await;
    let encoder = Arc::new(PanicsAt { panics_at: 1, encoded: AtomicUsize::new(0) });
    let (w, m) = writer_to(&broker, encoder);
    let restarts_before = RESTARTS.count(CDR_DRAINER);
    let buf = BufferedCdrWriter::spawn(Arc::new(w), 64, m.clone());
    for _ in 0..3 {
        buf.write(&a_call(), 2_000).await;
    }
    settle(&m, (2, 1)).await;
    assert_eq!(m.cdr_drops(), [(CdrDrop::Panic, 1)], "drops by reason");
    assert_eq!(broker.count(|c| &c.accepted), 1, "one connection served the three");
    assert_eq!(broker.count(|c| &c.ended_by_client), 0, "the session served on");
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::WriterPanic), 0);
    assert_eq!(RESTARTS.count(CDR_DRAINER) - restarts_before, 1, "the caught panic is counted");
    buf.close(Instant::now() + 500 * MS).await;
    assert_eq!(counts(&m), (2, 1), "the close counts nothing twice");
}

/// The observer is told one end per record handed to the writer, the record
/// whose write panicked included, in the order the records ended.
#[tokio::test]
async fn the_observer_is_told_the_end_of_a_record_whose_write_panicked() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let encoder = Arc::new(PanicsAt { panics_at: 1, encoded: AtomicUsize::new(0) });
    let (w, m) = writer_to(&broker, encoder);
    let ends = Arc::new(Ends { panics: AtomicUsize::new(0), told: Default::default() });
    let buf = BufferedCdrWriter::spawn(Arc::new(w.observed_by(ends.clone())), 64, m.clone());
    buf.write(&a_call(), 2_000).await;
    settle(&m, (1, 0)).await;
    buf.write(&a_call(), 2_000).await;
    settle(&m, (1, 1)).await;
    buf.write(&a_call(), 2_000).await;
    settle(&m, (2, 1)).await;
    let told = ends.told.lock().unwrap().clone();
    assert_eq!(told, [CdrDelivery::Written, CdrDelivery::Dropped, CdrDelivery::Written]);
}

/// The observer panics on its first two calls: each record still counts
/// once, as the broker answered it, and the session serves on.
#[tokio::test]
async fn an_observer_that_panics_costs_neither_the_count_nor_the_session() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let encoder = Arc::new(PanicsAt { panics_at: usize::MAX, encoded: AtomicUsize::new(0) });
    let (w, m) = writer_to(&broker, encoder);
    let ends = Arc::new(Ends { panics: AtomicUsize::new(2), told: Default::default() });
    let w = w.observed_by(ends.clone());
    for written in 1..=3 {
        w.write(&a_call(), 2_000).await;
        settle(&m, (written, 0)).await;
    }
    assert_eq!(*ends.told.lock().unwrap(), [CdrDelivery::Written], "the third end, told");
    assert_eq!(broker.count(|c| &c.accepted), 1, "one connection served the three");
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::TrackerPanic), 0);
    w.close(Instant::now() + 500 * MS).await;
    counted(&broker, |c| &c.ended_by_client, 1).await;
}
