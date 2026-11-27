//! Forward writes of a live takeover copy's call, applied faster than the
//! router handles them, ride up within the flush budget: the puller stops
//! applying once the bodies it handed up fill the budget, goes on as the
//! router handles them, and every write rides up, in order (ADR-0047 D7).
//!
//! ```text
//!   stream    a warm Backup flow: 8 forward Puts of one call, gens 1..8,
//!             1 MiB bodies, one frame every 10 ms
//!   budget    3 MiB: three bodies
//!   router    reads nothing for 3 s, then one command, then the rest
//!   puller    answers each applied frame with its Position, after the body
//!             rode up; idle bound 2 s, so the 3 s wait on the budget must not
//!             read as a silent stream
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

const MIB: usize = 1024 * 1024;
const COPY: &str = "w0|copy|t";
const WRITES: u64 = 8;

/// The primary's write `gen` of the copy's call: a 1 MiB body of `gen` bytes.
fn write(gen: u64) -> Frame {
    Frame::Data {
        at: at(gen),
        op: Op::Put,
        partition: Partition::Bak,
        call_ref: COPY.to_string(),
        call_gen: gen as i64,
        call_bgen: 0,
        body_ttl_ms: 0,
        origin_now_ms: 0,
        indexes: Vec::new(),
        body: Some(Arc::from(vec![gen as u8; MIB])),
        answered: false,
        incarnation: None,
    }
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

async fn advance(ms: u64) {
    for _ in 0..ms / 10 {
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn forward_writes_ride_up_within_the_budget_and_none_is_lost() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let script: VecDeque<Frame> = (1..=WRITES).map(write).collect();
    let net: Arc<dyn ReplicationNetwork> =
        Arc::new(RecordingNet { script: Mutex::new(Some(script)), sent: sent.clone() });
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        Partition::Bak,
        "127.0.0.1:9".parse().unwrap(),
        net,
        store,
        PullerConfig::fast_test(),
        Watermark::new(1, 0),
        B2buaMetrics::new(),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let probe: crate::store::TakeoverProbe = Arc::new(|r: &str| r == COPY);
    let budget = FlushBudget::new(3 * MIB);
    let puller = puller.with_repl_sink(RouterSink::new(tx, probe).with_budget(budget.clone()));
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let task = tokio::spawn(async move { puller.run(cancel_rx).await });

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
    for _ in 0..WRITES * 2 {
        while let Ok(cmd) = rx.try_recv() {
            gens.push(gen_of(&cmd));
        }
        advance(10).await;
    }
    assert_eq!(gens, (1..=WRITES as u8).collect::<Vec<_>>(), "every write rides up, in order");
    let want: Vec<Frame> = (1..=WRITES).map(position).collect();
    assert_eq!(positions(&sent), want);
    assert_eq!(budget.in_use(), 0, "every handled body gave its bytes back");

    let _ = cancel_tx.send(true);
    task.abort();
}
