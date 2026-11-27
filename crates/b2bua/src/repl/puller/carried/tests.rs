//! The sweep of a bootstrap's uncarried copies, on scripted passes: `w1`
//! bootstraps a flow from `w0`, whose streams each serve one script, a frame
//! every 10 ms, then cut or fall quiet. Every pass is cold; the second one
//! opens after the backoff (100 ms or more) that follows the first's cut.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use call::{CallModelState, LegState};
use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};
use sip_clock::Clock;
use tokio::sync::{mpsc, watch};

use super::{CarriedCopies, StoredCopy};
use crate::metrics::B2buaMetrics;
use crate::repl::call_bodies_tests::{body_at, call_in};
use crate::repl::{Puller, PullerConfig, ReplicatingCallStore, RouterSink};
use crate::router::{PrimaryWrite, ReplCommand};
use crate::store::{CallStore, PartitionRole, PutOpts, TakeoverProbe};

/// What a scripted stream does once its frames are read.
#[derive(Clone, Copy)]
enum End {
    /// The link drops: the puller reads the stream's end.
    Cut,
    /// The stream stays open and sends nothing more.
    Quiet,
}

/// One stream `w0` serves.
struct Pass {
    frames: Vec<Frame>,
    end: End,
}

struct PassConn {
    frames: Mutex<VecDeque<Frame>>,
    end: End,
}

#[async_trait]
impl ReplicationConnection for PassConn {
    async fn send(&self, _frame: Frame) -> Result<(), SendError> {
        Ok(())
    }
    async fn recv(&self) -> Option<Frame> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let next = self.frames.lock().unwrap().pop_front();
        match (next, self.end) {
            (Some(frame), _) => Some(frame),
            (None, End::Cut) => None,
            (None, End::Quiet) => std::future::pending().await,
        }
    }
    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }
    fn local_addr(&self) -> SocketAddr {
        "127.0.0.1:8".parse().unwrap()
    }
}

/// `w0`: one scripted pass per connect, a refused connect once all are served.
struct Holder {
    passes: Mutex<VecDeque<Pass>>,
}

#[async_trait]
impl ReplicationNetwork for Holder {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let Some(pass) = self.passes.lock().unwrap().pop_front() else {
            return Err(ConnectError::Refused(dst));
        };
        Ok(Box::new(PassConn { frames: Mutex::new(pass.frames.into()), end: pass.end }))
    }
    async fn listen(
        &self,
        _local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        unreachable!("the holder is pull-only")
    }
}

const FLOWS: [Partition; 2] = [Partition::Bak, Partition::Pri];

/// The key of call `id` on `flow`: a `w0` call `w1` backs up, or a `w1` call
/// `w1` recovers.
fn key(flow: Partition, id: &str) -> String {
    match flow {
        Partition::Bak => format!("w0|{id}|t"),
        Partition::Pri => format!("w1|{id}|t"),
    }
}

/// `w0`'s `Put` of call `id` at `(gen, counter)` of its changelog, version
/// `(p, 0)`.
fn put(flow: Partition, (gen, counter): (u64, u64), id: &str, p: i64) -> Frame {
    Frame::Data {
        at: Watermark::new(gen, counter),
        op: Op::Put,
        partition: flow,
        call_ref: key(flow, id),
        call_gen: p,
        call_bgen: 0,
        body_ttl_ms: 0,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: Some(Arc::from(b"body".to_vec().into_boxed_slice())),
        answered: false,
        incarnation: None,
        discharged: false,
    }
}

fn noop((gen, counter): (u64, u64)) -> Frame {
    Frame::Noop { at: Watermark::new(gen, counter) }
}

fn cut(frames: Vec<Frame>) -> Pass {
    Pass { frames, end: End::Cut }
}

fn quiet(frames: Vec<Frame>) -> Pass {
    Pass { frames, end: End::Quiet }
}

/// Advance the paused clock `ms` in 10 ms steps, one scripted frame a step.
async fn pump(ms: u64) {
    for _ in 0..ms / 10 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
}

/// `w1` bootstrapping `flow` from `w0`: its store, metrics, the record of
/// its passes, the calls its router serves as live takeover copies and the
/// router commands its pullers send through the node's one sink.
struct Node {
    flow: Partition,
    store: ReplicatingCallStore,
    metrics: B2buaMetrics,
    carried: CarriedCopies,
    takeovers: Arc<Mutex<HashSet<String>>>,
    sink: RouterSink,
    rx: mpsc::UnboundedReceiver<ReplCommand>,
}

impl Node {
    fn new(flow: Partition) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let takeovers: Arc<Mutex<HashSet<String>>> = Arc::default();
        let served = takeovers.clone();
        let probe: TakeoverProbe = Arc::new(move |r: &str| served.lock().unwrap().contains(r));
        Self {
            flow,
            store: ReplicatingCallStore::new(1, Clock::test_at(0)),
            metrics: B2buaMetrics::new(),
            carried: CarriedCopies::default(),
            takeovers,
            sink: RouterSink::new(tx, probe),
            rx,
        }
    }

    /// Start a puller of the flow from `w0` over `passes`; its cancel handle.
    fn pull(&self, passes: Vec<Pass>, config: PullerConfig) -> watch::Sender<bool> {
        self.pull_from("w0", self.carried.clone(), passes, config)
    }

    /// Start a puller of the flow from `peer` over `passes`, its record of
    /// the passes `carried`; its cancel handle.
    fn pull_from(
        &self,
        peer: &str,
        carried: CarriedCopies,
        passes: Vec<Pass>,
        config: PullerConfig,
    ) -> watch::Sender<bool> {
        let holder = Holder { passes: Mutex::new(passes.into()) };
        let (puller, _status) = Puller::new_at(
            peer,
            "w1",
            self.flow,
            "127.0.0.1:9".parse().unwrap(),
            Arc::new(holder),
            self.store.clone(),
            config,
            Watermark::new(0, 0),
            self.metrics.clone(),
        );
        let puller = puller.with_repl_sink(self.sink.clone()).with_carried(carried);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        tokio::spawn(puller.run(cancel_rx));
        cancel_tx
    }

    fn pull_plain(&self, passes: Vec<Pass>) -> watch::Sender<bool> {
        self.pull(passes, PullerConfig::fast_test())
    }

    /// The router serves call `id` as a live takeover copy.
    fn serve_as_takeover(&self, id: &str) {
        self.takeovers.lock().unwrap().insert(key(self.flow, id));
    }

    fn keyspace(&self) -> (PartitionRole, &'static str) {
        match self.flow {
            Partition::Bak => (PartitionRole::Backup, "w0"),
            Partition::Pri => (PartitionRole::Primary, "w1"),
        }
    }

    async fn holds(&self, id: &str) -> bool {
        let (role, primary) = self.keyspace();
        self.store.get_call(role, primary, &key(self.flow, id)).await.unwrap().is_some()
    }

    /// Write call `id` at version `(p, b)` here, as the router or a takeover
    /// copy writes it.
    async fn write(&self, id: &str, version: (i64, i64)) {
        self.write_body(id, b"written here".to_vec(), version).await;
    }

    /// [`write`](Self::write) of `body`.
    async fn write_body(&self, id: &str, body: Vec<u8>, (p, b): (i64, i64)) {
        let (role, primary) = self.keyspace();
        let opts = PutOpts::default();
        self.store
            .put_call(role, primary, &key(self.flow, id), body, &[], 0, p, b, &opts)
            .await
            .unwrap();
    }

    fn swept(&self) -> u64 {
        let flow = match self.flow {
            Partition::Bak => "backup",
            Partition::Pri => "recovery",
        };
        self.metrics.repl_bootstrap_swept(flow)
    }

    /// The drop notices, delete notices of a takeover copy and bulk reclaims
    /// the router was sent, in order.
    fn reclaim_commands(&mut self) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ReplCommand::ReverseDeleteApplied { call_ref, .. } => {
                    seen.push(format!("dropped {call_ref}"));
                }
                ReplCommand::ForwardFlushApplied { call_ref, write: PrimaryWrite::Delete(_) } => {
                    seen.push(format!("delete heard by {call_ref}"));
                }
                ReplCommand::ReclaimAll => seen.push("reclaim all".to_string()),
                _ => {}
            }
        }
        seen
    }
}

/// Pass 1 carries X and is cut; X ends on `w0`; pass 2 carries Y and catches
/// up: X's copy goes, Y's stays, on either flow, and the one sweep is counted.
/// On the Reclaim flow the router reads X dropped before the bulk reclaim.
#[tokio::test(start_paused = true)]
async fn a_copy_an_earlier_pass_carried_and_the_last_did_not_is_dropped() {
    for flow in FLOWS {
        let mut node = Node::new(flow);
        let _cancel = node.pull_plain(vec![
            cut(vec![put(flow, (1, 5), "x", 1)]),
            quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
        ]);
        pump(1_000).await;

        assert!(!node.holds("x").await, "{flow:?}: X ended between the passes, its copy goes");
        assert!(node.holds("y").await, "{flow:?}: Y, carried by the last pass, stays");
        assert_eq!(node.swept(), 1, "{flow:?}: one copy swept");
        if flow == Partition::Pri {
            let x = key(flow, "x");
            assert_eq!(node.reclaim_commands(), [format!("dropped {x}"), "reclaim all".into()]);
        }
    }
}

/// The holder restarted between the passes: pass 2 is of another
/// incarnation, whose scan may lack a call the earlier one still holds (not
/// yet recovered), so nothing pass 1 carried is dropped, whether pass 2
/// carries another call or, freshly restarted, nothing at all.
#[tokio::test(start_paused = true)]
async fn a_pass_of_another_holder_incarnation_drops_nothing() {
    for flow in FLOWS {
        for second in [vec![put(flow, (2, 3), "y", 1), noop((2, 3))], vec![noop((2, 1))]] {
            let node = Node::new(flow);
            let _cancel =
                node.pull_plain(vec![cut(vec![put(flow, (1, 5), "x", 1)]), quiet(second)]);
            pump(1_000).await;

            assert!(
                node.holds("x").await,
                "{flow:?}: X, carried by the earlier incarnation, stays"
            );
            assert_eq!(node.swept(), 0, "{flow:?}");
        }
    }
}

/// Pass 1 of incarnation 1 carries X; the holder restarts; pass 2, of
/// incarnation 2, carries Y and is cut; pass 3 catches up carrying nothing.
/// Only Y, which an earlier pass of the catching-up incarnation carried,
/// goes.
#[tokio::test(start_paused = true)]
async fn only_the_passes_of_the_incarnation_that_catches_up_are_swept() {
    for flow in FLOWS {
        let node = Node::new(flow);
        let _cancel = node.pull_plain(vec![
            cut(vec![put(flow, (1, 5), "x", 1)]),
            cut(vec![put(flow, (2, 3), "y", 1)]),
            quiet(vec![noop((2, 4))]),
        ]);
        pump(1_500).await;

        assert!(node.holds("x").await, "{flow:?}: X, of the earlier incarnation, stays");
        assert!(!node.holds("y").await, "{flow:?}: Y ended between passes 2 and 3");
        assert_eq!(node.swept(), 1, "{flow:?}");
    }
}

/// A stream opened after the current pass fences it out: a cancelled
/// puller that opens its stream late leaves its successor's pass recording
/// nothing and sweeping nothing, and a catch-up of a pass that is no longer
/// the current one sweeps nothing and leaves the record to the current one.
#[test]
fn only_the_stream_opened_last_records_and_sweeps() {
    let held = || Some(StoredCopy { version: (1, 0), incarnation: None });

    let record = CarriedCopies::default();
    let first = record.open_pass();
    record.carried(first, 1, "x", held());
    let successor = record.open_pass();
    record.carried(successor, 1, "x", held());
    let _late = record.open_pass();
    assert!(record.caught_up(successor, 1).is_empty(), "X, carried by the successor, stays");

    let record = CarriedCopies::default();
    let stale = record.open_pass();
    record.carried(stale, 1, "x", held());
    let current = record.open_pass();
    assert!(record.caught_up(stale, 1).is_empty(), "a stale catch-up sweeps nothing");
    let swept: Vec<String> = record.caught_up(current, 1).into_iter().map(|(k, _)| k).collect();
    assert_eq!(swept, ["x"], "the current pass sweeps what the earlier one left");
}

/// The last pass carries a key in any frame: X at a version the stored copy
/// dominates, in the scan; Z only on the tail, past the scan's position,
/// before the catch-up `Noop`. Both stay.
#[tokio::test(start_paused = true)]
async fn a_key_the_last_pass_carried_in_any_frame_stays() {
    for flow in FLOWS {
        let node = Node::new(flow);
        let _cancel = node.pull_plain(vec![
            cut(vec![put(flow, (1, 5), "x", 2), put(flow, (1, 5), "z", 1)]),
            quiet(vec![
                put(flow, (1, 9), "x", 1),
                put(flow, (1, 9), "y", 1),
                put(flow, (1, 10), "z", 2),
                noop((1, 10)),
            ]),
        ]);
        pump(1_000).await;

        for id in ["x", "y", "z"] {
            assert!(node.holds(id).await, "{flow:?}: {id} was carried by the last pass");
        }
        assert_eq!(node.swept(), 0, "{flow:?}");
    }
}

/// Pass 1 carries X and R and is cut; R is written here between the passes;
/// W, a call this node recovered from another peer, is written here too.
/// Pass 2 carries Y alone and catches up: X goes; R, no longer the copy
/// pass 1 left, stays, as does W, which this flow never carried.
#[tokio::test(start_paused = true)]
async fn on_the_reclaim_flow_a_copy_written_since_its_pass_stays() {
    let flow = Partition::Pri;
    let mut node = Node::new(flow);
    let _cancel = node.pull_plain(vec![
        cut(vec![put(flow, (1, 5), "x", 1), put(flow, (1, 5), "r", 1)]),
        quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
    ]);
    pump(40).await;
    assert!(node.holds("r").await, "pass 1 landed before its cut");
    node.write("r", (1, 1)).await;
    node.write("w", (1, 0)).await;
    pump(1_000).await;

    assert!(!node.holds("x").await, "X goes");
    assert!(node.holds("r").await, "R was written since pass 1 carried it");
    assert!(node.holds("w").await, "W was never carried by this flow");
    assert_eq!(node.swept(), 1);
    let x = key(flow, "x");
    assert_eq!(node.reclaim_commands(), [format!("dropped {x}"), "reclaim all".into()]);
}

/// On the Backup flow the sweep drops a copy as the holder's tail `Delete`
/// would have: whatever this node wrote on it since, and with the router told
/// of the delete for a call it serves as a live takeover copy (ADR-0047 D7).
/// Pass 1 carries X, R and T and is cut; R is written here between the
/// passes, and the router serves T as a live takeover copy. Pass 2 carries Y
/// alone and catches up: X, R and T go; T's copy alone is told of the delete.
#[tokio::test(start_paused = true)]
async fn on_the_backup_flow_the_sweep_drops_as_a_tail_delete_does() {
    let flow = Partition::Bak;
    let mut node = Node::new(flow);
    node.serve_as_takeover("t");
    let _cancel = node.pull_plain(vec![
        cut(vec![put(flow, (1, 5), "x", 1), put(flow, (1, 5), "r", 1), put(flow, (1, 5), "t", 1)]),
        quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
    ]);
    pump(60).await;
    assert!(node.holds("t").await, "pass 1 landed before its cut");
    node.write("r", (1, 1)).await;
    node.write("t", (1, 1)).await;
    pump(1_000).await;

    for id in ["x", "r", "t"] {
        assert!(!node.holds(id).await, "{id} goes");
    }
    assert!(node.holds("y").await);
    assert_eq!(node.swept(), 3);
    assert_eq!(node.reclaim_commands(), [format!("delete heard by {}", key(flow, "t"))]);
}

/// The sweep keeps an Element the `Delete` guard keeps (ADR-0031 D3): pass 1
/// carries X and Z, ringing, and is cut; a takeover copy here answers both
/// callers and writes their Elements, which the holder never reads, Z's then
/// ending; the holder ends both behind the cut. Pass 2 carries Y alone and
/// catches up: X's Element, `Active` with its caller answered here, stays;
/// Z's, ending, goes; no takeover copy is live to hear of either.
#[tokio::test(start_paused = true)]
async fn on_the_backup_flow_the_sweep_keeps_an_element_answered_here() {
    let flow = Partition::Bak;
    let mut node = Node::new(flow);
    let _cancel = node.pull_plain(vec![
        cut(vec![put(flow, (1, 5), "x", 1), put(flow, (1, 5), "z", 1)]),
        quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
    ]);
    pump(40).await;
    assert!(node.holds("z").await, "pass 1 landed before its cut");
    let answered = call_in("w0", "w1", "x", LegState::Confirmed, CallModelState::Active);
    node.write_body("x", body_at(&answered, 1, 1), (1, 1)).await;
    let ending = call_in("w0", "w1", "z", LegState::Confirmed, CallModelState::Terminating);
    node.write_body("z", body_at(&ending, 1, 1), (1, 1)).await;
    pump(1_000).await;

    assert!(node.holds("x").await, "X's Element, answered here, stays");
    assert!(!node.holds("z").await, "Z's Element, ending, goes");
    assert!(node.holds("y").await);
    assert_eq!(node.swept(), 1);
    assert!(node.reclaim_commands().is_empty());
}

/// On the Reclaim flow a hard timer running out on an open pass reclaims a
/// partial seed: that bootstrap sweeps nothing, even once a later pass
/// catches up without X (the stated residual). Pass 1 lands X at 10 ms and
/// falls quiet; the 500 ms timer fires; the 2 s idle bound leaves the stream;
/// pass 2 catches up.
#[tokio::test(start_paused = true)]
async fn a_reclaim_whose_hard_timer_reclaimed_a_partial_seed_sweeps_nothing() {
    let mut node = Node::new(Partition::Pri);
    let flow = Partition::Pri;
    let config = PullerConfig { bootstrap_hard_timeout_ms: 500, ..PullerConfig::fast_test() };
    let _cancel = node.pull(
        vec![
            quiet(vec![put(flow, (1, 5), "x", 1)]),
            quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
        ],
        config,
    );
    pump(3_000).await;

    assert!(node.holds("x").await, "X stays: the timer's bulk reclaim may serve it");
    assert!(node.holds("y").await, "pass 2 caught up");
    assert_eq!(node.swept(), 0);
    assert_eq!(node.reclaim_commands(), ["reclaim all", "reclaim all"]);
}

/// A bulk reclaim materialises the copies of every Reclaim flow: one another
/// flow fires while this flow's record holds a copy may serve it, so this
/// bootstrap sweeps nothing (the residual of ADR-0014 §8), and one fired
/// while the record held nothing leaves the sweep whole. The flow from `w2`
/// catches up 10 ms after its spawn, 50 ms before or after the flow from
/// `w0` opens; that one lands X at 10 ms, is cut at 20 ms and catches up
/// without X past the 100 ms backoff, so X is held from 10 ms to then.
#[tokio::test(start_paused = true)]
async fn a_bulk_reclaim_of_another_flow_while_a_pass_holds_a_copy_leaves_its_sweep_out() {
    let flow = Partition::Pri;
    for other_before in [false, true] {
        let mut node = Node::new(flow);
        let other = || {
            let catches_up = vec![quiet(vec![noop((1, 1))])];
            node.pull_from("w2", CarriedCopies::default(), catches_up, PullerConfig::fast_test())
        };
        let mut started = Vec::new();
        if other_before {
            started.push(other());
            pump(50).await;
        }
        started.push(node.pull_plain(vec![
            cut(vec![put(flow, (1, 5), "x", 1)]),
            quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
        ]));
        if !other_before {
            pump(50).await;
            started.push(other());
        }
        pump(1_000).await;

        assert_eq!(node.holds("x").await, !other_before, "other before: {other_before}");
        assert!(node.holds("y").await);
        assert_eq!(node.swept(), u64::from(other_before));
        let x_dropped = other_before.then(|| format!("dropped {}", key(flow, "x")));
        let mut want = vec!["reclaim all".to_string()];
        want.extend(x_dropped);
        want.push("reclaim all".to_string());
        assert_eq!(node.reclaim_commands(), want, "other before: {other_before}");
    }
}

/// A puller spawned between two passes reads what the earlier ones carried
/// from the flow's record: the second puller's catch-up drops X.
#[tokio::test(start_paused = true)]
async fn a_puller_spawned_between_two_passes_sweeps_what_the_earlier_carried() {
    let flow = Partition::Bak;
    let node = Node::new(flow);
    let first = node.pull_plain(vec![cut(vec![put(flow, (1, 5), "x", 1)])]);
    pump(60).await;
    assert!(node.holds("x").await, "pass 1 landed X");
    first.send(true).unwrap();
    pump(20).await;

    let _second = node.pull_plain(vec![quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))])]);
    pump(500).await;
    assert!(!node.holds("x").await, "X goes");
    assert!(node.holds("y").await);
    assert_eq!(node.swept(), 1);
}
