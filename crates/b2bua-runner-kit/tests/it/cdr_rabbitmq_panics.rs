//! The RabbitMQ CDR sink through a panic: a panic of the writer's work ends
//! its session, the next record opens a fresh one, and every record counts
//! once.

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

/// The drainer catches a panic inside the writer's `write`: that record
/// counts dropped once, the writer ends its session, and the next record is
/// written over a new connection.
#[tokio::test]
async fn after_a_caught_write_panic_the_next_record_opens_a_fresh_session() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let encoder = Arc::new(PanicsAt { panics_at: 1, encoded: AtomicUsize::new(0) });
    let (w, m) = writer_to(&broker, encoder);
    let restarts_before = RESTARTS.count(CDR_DRAINER);
    let buf = BufferedCdrWriter::spawn(Arc::new(w), 64, m.clone());
    buf.write(&a_call(), 2_000).await;
    settle(&m, (1, 0)).await;
    buf.write(&a_call(), 2_000).await;
    settle(&m, (1, 1)).await;
    counted(&broker, |c| &c.ended_by_client, 1).await;
    buf.write(&a_call(), 2_000).await;
    settle(&m, (2, 1)).await;
    assert_eq!(broker.count(|c| &c.accepted), 2, "the record after the panic reconnected");
    assert_eq!(RESTARTS.count(CDR_DRAINER) - restarts_before, 1, "the caught panic is counted");
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::WriterPanic), 1);
    buf.close(Instant::now() + 500 * MS).await;
    assert_eq!(counts(&m), (2, 1), "the close counts nothing twice");
    assert_eq!(m.cdr_drops(), [(CdrDrop::Panic, 1)]);
}

/// The confirm tracker panics while it counts an acked record (its observer
/// panics): the session ends at once, before any further record, the record
/// counts dropped once, and the next record is written over a new connection.
#[tokio::test]
async fn a_panic_of_the_confirm_tracker_ends_its_session_at_once_and_counts_its_record_once() {
    let broker = FakeBroker::start(Reply::Ack).await;
    let encoder = Arc::new(PanicsAt { panics_at: usize::MAX, encoded: AtomicUsize::new(0) });
    let (w, m) = writer_to(&broker, encoder);
    let ends = Arc::new(Ends { panics: AtomicUsize::new(1), told: Default::default() });
    let w = w.observed_by(ends.clone());
    w.write(&a_call(), 2_000).await;
    settle(&m, (0, 1)).await;
    counted(&broker, |c| &c.ended_by_client, 1).await;
    assert_eq!(*ends.told.lock().unwrap(), vec![CdrDelivery::Dropped], "told one end");
    assert_eq!(m.cdr_drops(), [(CdrDrop::Panic, 1)]);
    assert_eq!(m.cdr_broker_session_ends_of(BrokerSessionEnd::TrackerPanic), 1);
    w.write(&a_call(), 2_000).await;
    settle(&m, (1, 1)).await;
    assert_eq!(broker.count(|c| &c.accepted), 2);
}
