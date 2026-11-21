//! The drainer through panics of the sink it drives: every record counted
//! once, the worker's vital list never seeing the drainer end, and a close
//! ending by its deadline.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_txn::LoopPulse;
use task_restart::RESTART_DELAY;

use super::*;
use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::loops::WorkerLoops;
use crate::supervise::{CDR_DRAINER, RESTARTS};

const MS: Duration = Duration::from_millis(1);

fn a_call() -> Call {
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 192.0.2.9:5060;branch=z9hG4bK-panic\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: panic@192.0.2.9\r\n\
        CSeq: 1 INVITE\r\n\
        Contact: <sip:alice@192.0.2.9:5060>\r\n\
        Content-Length: 0\r\n\r\n";
    let req = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    };
    let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(
        &req,
        std::net::SocketAddr::from(([192, 0, 2, 9], 5060)),
        &cfg,
        &mut sip_ids::TurnIds::fixed(1),
        1_000,
    )
}

/// Takes `per_record` to write each record. Its next `write_panics` writes
/// panic before they write, and its next `hook_panics` calls of
/// `after_panic` panic; it remembers each `after_panic` call and its close.
struct PanickySink {
    per_record: Duration,
    write_panics: AtomicUsize,
    hook_panics: AtomicUsize,
    hooks: AtomicUsize,
    written: InMemoryCdrWriter,
    closed_by: std::sync::Mutex<Option<Instant>>,
}

impl PanickySink {
    fn new(per_record: Duration, write_panics: usize, hook_panics: usize) -> Arc<Self> {
        Arc::new(Self {
            per_record,
            write_panics: AtomicUsize::new(write_panics),
            hook_panics: AtomicUsize::new(hook_panics),
            hooks: AtomicUsize::new(0),
            written: InMemoryCdrWriter::new(),
            closed_by: Default::default(),
        })
    }

    fn written(&self) -> usize {
        self.written.snapshot().len()
    }

    fn hooks(&self) -> usize {
        self.hooks.load(Ordering::SeqCst)
    }

    fn closed_by(&self) -> Option<Instant> {
        *self.closed_by.lock().unwrap()
    }
}

/// Takes one from `left` when it is not zero.
fn take(left: &AtomicUsize) -> bool {
    left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
}

#[async_trait]
impl CdrWriter for PanickySink {
    async fn write(&self, call: &Call, terminated_at: i64) {
        if take(&self.write_panics) {
            panic!("expected: the sink's write panics");
        }
        tokio::time::sleep(self.per_record).await;
        self.written.write(call, terminated_at).await;
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.written.read_all().await
    }

    async fn close(&self, deadline: Instant) {
        *self.closed_by.lock().unwrap() = Some(deadline);
    }

    async fn after_panic(&self) {
        self.hooks.fetch_add(1, Ordering::SeqCst);
        if take(&self.hook_panics) {
            panic!("expected: the sink's after_panic panics");
        }
    }
}

/// A task that never ends.
fn parked() -> tokio::task::AbortHandle {
    tokio::spawn(std::future::pending::<()>()).abort_handle()
}

/// Loops that never end beside `buf`'s drainer as the vital `cdr_drainer`,
/// as the worker holds it.
fn worker_loops(buf: &BufferedCdrWriter) -> WorkerLoops {
    let pulse = || Arc::new(LoopPulse::new());
    let drainer = buf.task().expect("a buffering writer has a drainer");
    WorkerLoops::new((pulse(), parked()), (pulse(), parked())).with_vital("cdr_drainer", drainer)
}

struct Rig {
    sink: Arc<PanickySink>,
    buf: BufferedCdrWriter,
    metrics: B2buaMetrics,
    loops: WorkerLoops,
    restarts_before: u64,
}

impl Rig {
    /// `sink` behind a buffer, `records` queued.
    async fn new(sink: Arc<PanickySink>, records: usize) -> Self {
        let restarts_before = RESTARTS.count(CDR_DRAINER);
        let metrics = B2buaMetrics::new();
        let buf = BufferedCdrWriter::spawn(sink.clone(), 64, metrics.clone());
        let loops = worker_loops(&buf);
        for _ in 0..records {
            buf.write(&a_call(), 2_000).await;
        }
        Self { sink, buf, metrics, loops, restarts_before }
    }

    fn dropped(&self) -> u64 {
        assert_eq!(self.buf.dropped_total(), self.metrics.cdr_dropped_total());
        self.buf.dropped_total()
    }

    fn restarts(&self) -> u64 {
        RESTARTS.count(CDR_DRAINER) - self.restarts_before
    }
}

#[tokio::test(start_paused = true)]
async fn a_write_that_panics_counts_its_record_dropped_once_and_the_next_records_are_written() {
    let rig = Rig::new(PanickySink::new(Duration::ZERO, 1, 0), 3).await;
    tokio::time::sleep(10 * MS).await;
    assert_eq!((rig.sink.written(), rig.dropped()), (2, 1));
    assert_eq!(rig.restarts(), 1, "the caught panic is counted");
    assert_eq!(rig.sink.hooks(), 1, "the sink dropped its partial state once");
    assert_eq!(rig.loops.ended(), None, "the worker serves on");
    rig.buf.close(Instant::now() + Duration::from_secs(1)).await;
    assert_eq!((rig.sink.written(), rig.dropped()), (2, 1), "the close counts nothing twice");
    assert_eq!(rig.metrics.cdr_drops(), [(CdrDrop::Panic, 1)]);
}

/// The sink's `after_panic` panics after a caught write panic: the run ends,
/// and the records queued behind it wait out the restart delay, then the new
/// run tells the sink first and writes them.
#[tokio::test(start_paused = true)]
async fn a_run_that_panics_is_made_again_after_the_restart_delay_and_writes_the_queue() {
    let rig = Rig::new(PanickySink::new(Duration::ZERO, 1, 1), 3).await;
    tokio::time::sleep(10 * MS).await;
    assert_eq!((rig.sink.written(), rig.dropped()), (0, 1));
    assert_eq!(rig.restarts(), 2, "the caught write panic and the run's panic");
    assert_eq!(rig.loops.ended(), None, "the worker serves on through the delay");
    tokio::time::sleep(RESTART_DELAY - 20 * MS).await;
    assert_eq!(rig.sink.written(), 0, "no run inside the restart delay");
    tokio::time::sleep(20 * MS).await;
    assert_eq!((rig.sink.written(), rig.dropped()), (2, 1), "the queue outlived the run");
    assert_eq!(rig.sink.hooks(), 2, "the new run told the sink first");
    assert_eq!(rig.loops.ended(), None);
}

/// A close begun inside the restart delay starts the next run at once: it
/// writes until the deadline, the write the deadline cuts and the records
/// behind it count dropped, and the close returns by its deadline.
#[tokio::test(start_paused = true)]
async fn a_close_during_the_restart_delay_ends_by_its_deadline_with_every_record_counted() {
    let rig = Rig::new(PanickySink::new(100 * MS, 1, 1), 10).await;
    tokio::time::sleep(10 * MS).await;
    assert_eq!(rig.restarts(), 2, "the drainer is in its restart delay");
    let deadline = Instant::now() + 350 * MS;
    rig.buf.close(deadline).await;
    assert!(Instant::now() <= deadline, "the close returned past its deadline");
    assert_eq!(rig.sink.written(), 3, "three writes of 100 ms fit before the deadline");
    assert_eq!(rig.dropped(), 7, "the panicked one, the one cut and the five queued");
    let drops = [(CdrDrop::CloseDeadline, 6), (CdrDrop::Panic, 1)];
    assert_eq!(rig.metrics.cdr_drops(), drops);
    assert_eq!(rig.sink.closed_by(), Some(deadline), "the sink closed by the same deadline");
}

/// A run that panics during the close is not made again: the records still
/// queued count dropped and the close returns at once.
#[tokio::test(start_paused = true)]
async fn a_run_that_panics_during_the_close_gives_the_queue_up_and_the_close_returns() {
    let rig = Rig::new(PanickySink::new(100 * MS, 1, 1), 5).await;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(10);
    rig.buf.close(deadline).await;
    assert_eq!(Instant::now(), start, "no restart delay inside a close");
    assert_eq!((rig.sink.written(), rig.dropped()), (0, 5));
    assert_eq!(rig.metrics.cdr_drops(), [(CdrDrop::Panic, 5)], "the panicked one and the queue");
    assert_eq!(rig.restarts(), 2, "the caught write panic and the run's panic");
    assert_eq!(rig.sink.closed_by(), Some(deadline));
    rig.buf.write(&a_call(), 3_000).await;
    assert_eq!(rig.dropped(), 6, "a write after the close is counted dropped");
    assert_eq!(rig.metrics.cdr_drops(), [(CdrDrop::Closed, 1), (CdrDrop::Panic, 5)]);
}
