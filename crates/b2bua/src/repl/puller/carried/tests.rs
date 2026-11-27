//! The sweep of a bootstrap's uncarried copies, on scripted passes: `w1`
//! bootstraps a flow from `w0`, whose streams each serve one script, a frame
//! every 10 ms, then cut or fall quiet. Every pass is cold; the second one
//! opens after the backoff (100 ms or more) that follows the first's cut.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};
use sip_clock::Clock;
use tokio::sync::{mpsc, watch};

use super::{CarriedCopies, StoredCopy};
use crate::metrics::B2buaMetrics;
use crate::repl::{Puller, PullerConfig, ReplicatingCallStore, RouterSink};
use crate::router::ReplCommand;
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
/// its passes and the router commands its pullers send.
struct Node {
    flow: Partition,
    store: ReplicatingCallStore,
    metrics: B2buaMetrics,
    carried: CarriedCopies,
    tx: mpsc::UnboundedSender<ReplCommand>,
    rx: mpsc::UnboundedReceiver<ReplCommand>,
}

impl Node {
    fn new(flow: Partition) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            flow,
            store: ReplicatingCallStore::new(1, Clock::test_at(0)),
            metrics: B2buaMetrics::new(),
            carried: CarriedCopies::default(),
            tx,
            rx,
        }
    }

    /// Start a puller of the flow over `passes`, the router serving the
    /// calls `probe` names as takeover copies; its cancel handle.
    fn pull(
        &self,
        passes: Vec<Pass>,
        config: PullerConfig,
        probe: TakeoverProbe,
    ) -> watch::Sender<bool> {
        let holder = Holder { passes: Mutex::new(passes.into()) };
        let (puller, _status) = Puller::new_at(
            "w0",
            "w1",
            self.flow,
            "127.0.0.1:9".parse().unwrap(),
            Arc::new(holder),
            self.store.clone(),
            config,
            Watermark::new(0, 0),
            self.metrics.clone(),
        );
        let puller = puller
            .with_repl_sink(RouterSink::new(self.tx.clone(), probe))
            .with_carried(self.carried.clone());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        tokio::spawn(puller.run(cancel_rx));
        cancel_tx
    }

    fn pull_plain(&self, passes: Vec<Pass>) -> watch::Sender<bool> {
        self.pull(passes, PullerConfig::fast_test(), Arc::new(|_: &str| false))
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
    async fn write(&self, id: &str, (p, b): (i64, i64)) {
        let (role, primary) = self.keyspace();
        let body = b"written here".to_vec();
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

    /// The drop notices and bulk reclaims the router was sent, in order.
    fn reclaim_commands(&mut self) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ReplCommand::ReverseDeleteApplied { call_ref, .. } => {
                    seen.push(format!("dropped {call_ref}"));
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

/// Pass 1 carries X, R and T; R is written here between the passes, and on
/// the Backup flow the router serves T as a live takeover copy. Pass 2
/// carries Y alone: X goes; R, no longer the copy pass 1 left, stays, and so
/// does T on the Backup flow. On the Reclaim flow, a call this node recovered
/// from another peer (W) was never carried by this flow and stays.
#[tokio::test(start_paused = true)]
async fn a_copy_written_since_its_pass_or_served_as_a_takeover_copy_stays() {
    for flow in FLOWS {
        let mut node = Node::new(flow);
        let t = key(flow, "t");
        let probe: TakeoverProbe = Arc::new(move |r: &str| r == t);
        let _cancel = node.pull(
            vec![
                cut(vec![
                    put(flow, (1, 5), "x", 1),
                    put(flow, (1, 5), "r", 1),
                    put(flow, (1, 5), "t", 1),
                ]),
                quiet(vec![put(flow, (1, 9), "y", 1), noop((1, 9))]),
            ],
            PullerConfig::fast_test(),
            probe,
        );
        pump(60).await;
        assert!(node.holds("t").await, "pass 1 landed before its cut");
        node.write("r", (1, 1)).await;
        node.write("w", (1, 0)).await;
        pump(1_000).await;

        assert!(!node.holds("x").await, "{flow:?}: X goes");
        assert!(node.holds("r").await, "{flow:?}: R was written since pass 1 carried it");
        assert!(node.holds("w").await, "{flow:?}: W was never carried by this flow");
        let takeover_spared = flow == Partition::Bak;
        assert_eq!(node.holds("t").await, takeover_spared, "{flow:?}: T as a takeover copy");
        assert_eq!(node.swept(), 2 - u64::from(takeover_spared), "{flow:?}");
        if flow == Partition::Pri {
            let dropped: Vec<String> = ["t", "x"].iter().map(|id| key(flow, id)).collect();
            let mut commands = node.reclaim_commands();
            assert_eq!(
                commands.pop().as_deref(),
                Some("reclaim all"),
                "the bulk reclaim comes last"
            );
            commands.sort();
            let want: Vec<String> = dropped.iter().map(|k| format!("dropped {k}")).collect();
            assert_eq!(commands, want);
        }
    }
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
        Arc::new(|_: &str| false),
    );
    pump(3_000).await;

    assert!(node.holds("x").await, "X stays: the timer's bulk reclaim may serve it");
    assert!(node.holds("y").await, "pass 2 caught up");
    assert_eq!(node.swept(), 0);
    assert_eq!(node.reclaim_commands(), ["reclaim all", "reclaim all"]);
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
