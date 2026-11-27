//! Bodies a puller hands the router faster than the router handles them ride
//! up within the flush budget: the puller stops once the bodies it handed up
//! fill the budget, goes on as the router handles them, and every command
//! rides up, in order. A wait on the budget is not a silent stream: the
//! puller stays on it past its idle bound (ADR-0047 D7, ADR-0045).
//!
//! ```text
//!   link      one frame every 10 ms
//!   budget    3 MiB: three 1 MiB bodies
//!   router    reads nothing for 3 s, then the commands as they come
//!   puller    idle bound 2 s; a Position after each applied frame, once its
//!             command rode up, and after each Noop, once the batch's held
//!             copies rode up
//!   forward   a warm Backup flow: 8 applied Puts of a live takeover copy's
//!             call, gens 1..8
//!   reverse   a warm Reclaim flow: 6 Puts of 6 calls the store dominates,
//!             held to the batch's Noop, then a Put of a new call
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::ReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::{mpsc, watch};

use super::{at, position, RecordingNet};
use crate::metrics::B2buaMetrics;
use crate::repl::{FlushBudget, Puller, PullerConfig, ReplicatingCallStore, RouterSink};
use crate::router::{PrimaryWrite, ReplCommand};
use crate::store::{CallStore, PartitionRole, PutOpts};

const MIB: usize = 1024 * 1024;
const COPY: &str = "w0|copy|t";
const WRITES: u64 = 8;
const DOMINATED: u64 = 6;
const AFTER: &str = "w1|after|t";

/// A `Put` of `call_ref` at `gen`, the change-log entry `counter`, with a
/// body of `len` bytes of `tag`.
fn put(partition: Partition, call_ref: &str, gen: u64, counter: u64, tag: u8, len: usize) -> Frame {
    Frame::Data {
        at: at(counter),
        op: Op::Put,
        partition,
        call_ref: call_ref.to_string(),
        call_gen: gen as i64,
        call_bgen: 0,
        body_ttl_ms: 0,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: Some(Arc::from(vec![tag; len])),
        answered: false,
        incarnation: None,
    }
}

/// A warm puller of `partition` on `w1`, pulling `w0` over `script` into
/// `store` and handing `sink` its commands; the frames it sends back.
fn spawn_puller(
    partition: Partition,
    script: VecDeque<Frame>,
    store: ReplicatingCallStore,
    sink: RouterSink,
) -> (Arc<Mutex<Vec<Frame>>>, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let net: Arc<dyn ReplicationNetwork> =
        Arc::new(RecordingNet { script: Mutex::new(Some(script)), sent: sent.clone() });
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        partition,
        "127.0.0.1:9".parse().unwrap(),
        net,
        store,
        PullerConfig::fast_test(),
        Watermark::new(1, 0),
        B2buaMetrics::new(),
    );
    let puller = puller.with_repl_sink(sink);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        puller.run(cancel_rx).await;
    });
    (sent, cancel_tx, task)
}

/// The positions the puller sent back, in order.
fn positions(sent: &Mutex<Vec<Frame>>) -> Vec<Frame> {
    sent.lock().unwrap().iter().filter(|f| matches!(f, Frame::Position { .. })).cloned().collect()
}

/// The gen of the body an applied forward write carries up.
fn gen_of(cmd: &ReplCommand) -> u8 {
    match cmd {
        ReplCommand::ForwardFlushApplied { call_ref, write: PrimaryWrite::Put(body) } => {
            assert_eq!(call_ref, COPY);
            assert_eq!(body.len(), MIB, "the whole body rides up");
            body[0]
        }
        other => panic!("expected an applied forward write: {other:?}"),
    }
}

/// What a reverse-flow command names: the refused copy's call and its body's
/// tag, or the reclaimed call.
fn reverse_of(cmd: &ReplCommand) -> String {
    match cmd {
        ReplCommand::ReverseFlushRefused { call_ref, body, .. } => {
            assert_eq!(body.len(), MIB, "the whole body rides up");
            format!("refused {call_ref} {}", body[0])
        }
        ReplCommand::ReclaimCall(call_ref) => format!("reclaim {call_ref}"),
        other => panic!("expected a reverse-flow command: {other:?}"),
    }
}

async fn advance(ms: u64) {
    for _ in 0..ms / 10 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
}

/// What `read` reads of every command `rx` gets while the clock runs `ms`
/// more, each command handled and dropped as it comes.
async fn drain<T>(
    rx: &mut mpsc::UnboundedReceiver<ReplCommand>,
    ms: u64,
    read: impl Fn(&ReplCommand) -> T,
) -> Vec<T> {
    let mut got = Vec::new();
    for _ in 0..ms / 10 {
        while let Ok(cmd) = rx.try_recv() {
            got.push(read(&cmd));
        }
        advance(10).await;
    }
    got
}

#[tokio::test(start_paused = true)]
async fn forward_writes_ride_up_within_the_budget_and_none_is_lost() {
    let script = (1..=WRITES).map(|n| put(Partition::Bak, COPY, n, n, n as u8, MIB)).collect();
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let probe: crate::store::TakeoverProbe = Arc::new(|r: &str| r == COPY);
    let budget = FlushBudget::new(3 * MIB);
    let sink = RouterSink::new(tx, probe).with_budget(budget.clone());
    let (sent, cancel, task) = spawn_puller(Partition::Bak, script, store, sink);

    advance(3_000).await;
    assert_eq!(
        positions(&sent),
        vec![position(1), position(2), position(3)],
        "the puller stops at the fourth write, three bodies filling the budget",
    );
    assert_eq!(budget.in_use(), 3 * MIB);

    let first = rx.try_recv().expect("the first write rode up");
    assert_eq!(gen_of(&first), 1);
    drop(first);
    advance(20).await;
    assert_eq!(positions(&sent).last(), Some(&position(4)), "a handled body lets the next up");
    assert_eq!(positions(&sent).len(), 4, "and only the next");

    let mut gens = vec![1];
    gens.extend(drain(&mut rx, WRITES * 20, gen_of).await);
    assert_eq!(gens, (1..=WRITES as u8).collect::<Vec<_>>(), "every write rides up, in order");
    let want: Vec<Frame> = (1..=WRITES).map(position).collect();
    assert_eq!(positions(&sent), want);
    assert_eq!(budget.in_use(), 0, "every handled body gave its bytes back");

    let _ = cancel.send(true);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_copies_a_batch_held_ride_up_within_the_budget_and_the_stream_goes_on() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let call = |n: u64| format!("w1|c{n}|t");
    for n in 1..=DOMINATED {
        let opts = PutOpts::default();
        store
            .put_call(PartitionRole::Primary, "w1", &call(n), vec![0], &[], 0, 5, 0, &opts)
            .await
            .unwrap();
    }
    let mut script: VecDeque<Frame> =
        (1..=DOMINATED).map(|n| put(Partition::Pri, &call(n), 1, n, n as u8, MIB)).collect();
    script.push_back(Frame::Noop { at: at(DOMINATED) });
    script.push_back(put(Partition::Pri, AFTER, 1, DOMINATED + 1, 0, 16));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let budget = FlushBudget::new(3 * MIB);
    let sink = RouterSink::from(tx).with_budget(budget.clone());
    let (sent, cancel, task) = spawn_puller(Partition::Pri, script, store.clone(), sink);

    advance(3_000).await;
    let tail: Vec<Frame> = (1..=DOMINATED).map(position).collect();
    assert_eq!(positions(&sent), tail, "the Noop waits for the held copies to ride up");
    assert_eq!(budget.in_use(), 3 * MIB);

    let got = drain(&mut rx, 400, reverse_of).await;
    let mut want: Vec<String> =
        (1..=DOMINATED).map(|n| format!("refused {} {n}", call(n))).collect();
    want.push(format!("reclaim {AFTER}"));
    assert_eq!(got, want, "every held copy rides up in order, then the stream goes on");
    let mut tail = tail;
    tail.extend([position(DOMINATED), position(DOMINATED + 1)]);
    assert_eq!(positions(&sent), tail);
    assert!(store.get_call(PartitionRole::Primary, "w1", AFTER).await.unwrap().is_some());
    assert_eq!(budget.in_use(), 0);

    let _ = cancel.send(true);
    task.abort();
}
