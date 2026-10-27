//! The outbox's order and release rules (ADR-0055 §3), on a paused clock and
//! a change log whose backup reports are driven by hand.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use sip_clock::Clock;

use super::{Effects, Outboxes};
use crate::metrics::B2buaMetrics;
use crate::repl::{Changelog, SyncLevel, WritePath};
use crate::store::HoldTarget;
use crate::turn_probe::Released;

const CAP: Duration = Duration::from_millis(100);
const PEER: &str = "w2";

/// What the entries sent, in the order they left.
type Sent = Arc<Mutex<Vec<(&'static str, Released)>>>;

struct Bench {
    log: Changelog,
    outboxes: Arc<Outboxes>,
    metrics: B2buaMetrics,
    sent: Sent,
    /// The backup's flow, connected while it is held.
    serving: Box<dyn std::any::Any>,
}

/// Outboxes at `sync` over a change log whose backup `PEER` is live and
/// bootstrapped.
fn at(sync: SyncLevel) -> Bench {
    let log = Changelog::new(1, Clock::test_at(0));
    let serving = log.serving(PEER, Partition::Bak);
    log.note_applied(PEER, None, Partition::Bak, Watermark::new(1, 0));
    let metrics = B2buaMetrics::new();
    let path = WritePath { sync, sync_cap: CAP, ..WritePath::default() };
    let outboxes =
        Arc::new(Outboxes::new(Some((path, log.clone())), metrics.clone(), Default::default()));
    Bench { log, outboxes, metrics, sent: Sent::default(), serving: Box::new(serving) }
}

impl Bench {
    /// Log a write of `call_ref` and name what an entry queued now waits for.
    fn write(&self, call_ref: &str) -> HoldTarget {
        let at = self.log.bump_put(PEER, call_ref, Partition::Bak);
        HoldTarget { peer: PEER.to_string(), partition: Partition::Bak, at }
    }

    fn apply(&self, counter: u64) {
        self.log.note_applied(PEER, None, Partition::Bak, Watermark::new(1, counter));
    }

    fn effects(&self, name: &'static str) -> Effects {
        let sent = self.sent.clone();
        Box::new(move |released| {
            Box::pin(async move { sent.lock().unwrap().push((name, released)) })
        })
    }

    async fn push(&self, call_ref: &str, wait: Option<HoldTarget>, name: &'static str) {
        self.outboxes.push(call_ref, wait, self.effects(name)).await;
    }

    fn sent(&self) -> Vec<(&'static str, Released)> {
        self.sent.lock().unwrap().clone()
    }
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_entry_leaves_once_its_backup_applied_the_write() {
    let b = at(SyncLevel::Applied);
    let wait = b.write("c1");
    b.push("c1", Some(wait), "200").await;
    settle().await;
    assert!(b.sent().is_empty(), "not applied yet");
    assert_eq!(b.outboxes.depth(), 1);
    b.apply(1);
    settle().await;
    assert_eq!(b.sent(), vec![("200", Released::Applied)]);
    assert_eq!(b.outboxes.depth(), 0);
    assert_eq!(b.metrics.repl_sync_capped_total(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_later_entry_never_overtakes_an_earlier_one() {
    let b = at(SyncLevel::Applied);
    let wait = b.write("c1");
    b.push("c1", Some(wait), "ack").await;
    b.push("c1", None, "bye").await;
    settle().await;
    assert!(b.sent().is_empty(), "the BYE waits behind the ACK");
    b.apply(1);
    settle().await;
    assert_eq!(b.sent(), vec![("ack", Released::Applied), ("bye", Released::AtOnce)]);
}

#[tokio::test(start_paused = true)]
async fn each_entry_leaves_at_its_own_cap_counted_and_flagged() {
    let b = at(SyncLevel::Applied);
    let first = b.write("c1");
    b.push("c1", Some(first), "first").await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let second = b.write("c1");
    b.push("c1", Some(second), "second").await;
    tokio::time::sleep(CAP - Duration::from_millis(60) - Duration::from_millis(1)).await;
    settle().await;
    assert!(b.sent().is_empty(), "before the first cap");
    tokio::time::sleep(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(b.sent(), vec![("first", Released::Capped)], "the first at its own cap");
    tokio::time::sleep(Duration::from_millis(59)).await;
    settle().await;
    assert_eq!(b.sent().len(), 1, "the second's cap counts from its own enqueue");
    tokio::time::sleep(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(b.sent()[1], ("second", Released::Capped));
    assert_eq!(b.metrics.repl_sync_capped_total(), 2, "each cap hit counted");
}

#[tokio::test(start_paused = true)]
async fn a_later_write_never_extends_an_earlier_entrys_wait_past_its_cap() {
    let b = at(SyncLevel::Applied);
    let first = b.write("c1");
    b.push("c1", Some(first), "first").await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    // The call's entry moves to the later write, which records the first too.
    b.write("c1");
    b.apply(1);
    settle().await;
    assert!(b.sent().is_empty(), "the write it waits for moved on");
    tokio::time::sleep(Duration::from_millis(40)).await;
    settle().await;
    assert_eq!(b.sent(), vec![("first", Released::Capped)], "released at its own cap");
}

#[tokio::test(start_paused = true)]
async fn another_calls_applied_write_never_releases_an_entry() {
    let b = at(SyncLevel::Applied);
    let a1 = b.write("a");
    b.push("a", Some(a1), "a").await;
    b.write("b");
    b.write("a");
    b.apply(2);
    settle().await;
    assert!(b.sent().is_empty(), "B's applied write does not stand for A's");
    b.apply(3);
    settle().await;
    assert_eq!(b.sent(), vec![("a", Released::Applied)]);
}

#[tokio::test(start_paused = true)]
async fn off_releases_at_once() {
    let b = at(SyncLevel::Off);
    let wait = b.write("c1");
    b.push("c1", Some(wait), "200").await;
    assert_eq!(b.sent(), vec![("200", Released::AtOnce)], "sent before the push returned");
    assert_eq!(b.outboxes.depth(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_call_with_no_backup_waits_for_nothing() {
    let b = at(SyncLevel::Applied);
    b.push("c1", None, "200").await;
    assert_eq!(b.sent(), vec![("200", Released::AtOnce)], "sent before the push returned");
}

#[tokio::test(start_paused = true)]
async fn an_entry_applied_already_leaves_at_once() {
    let b = at(SyncLevel::Applied);
    let wait = b.write("c1");
    b.apply(1);
    b.push("c1", Some(wait), "200").await;
    assert_eq!(b.sent(), vec![("200", Released::Applied)], "sent before the push returned");
}

#[tokio::test(start_paused = true)]
async fn a_released_call_sends_its_outbox_at_once() {
    let b = at(SyncLevel::Applied);
    let wait = b.write("c1");
    b.push("c1", Some(wait.clone()), "ack").await;
    b.push("c1", Some(wait), "bye").await;
    b.outboxes.release("c1").await;
    assert_eq!(b.sent(), vec![("ack", Released::CallReleased), ("bye", Released::CallReleased)]);
    tokio::time::timeout(Duration::from_millis(1), b.outboxes.drained("c1"))
        .await
        .expect("the outbox is gone once released");
    assert_eq!(b.outboxes.depth(), 0);
    tokio::time::sleep(CAP).await;
    assert_eq!(b.sent().len(), 2, "nothing sent twice");
    assert_eq!(b.metrics.repl_sync_capped_total(), 0, "a release is no cap hit");
}

#[tokio::test(start_paused = true)]
async fn a_backup_whose_flow_goes_down_releases_its_entries_unflagged_as_capped() {
    let Bench { log, outboxes, metrics, sent, serving } = at(SyncLevel::Applied);
    let at = log.bump_put(PEER, "c1", Partition::Bak);
    let wait = HoldTarget { peer: PEER.to_string(), partition: Partition::Bak, at };
    let effects: Effects = {
        let sent = sent.clone();
        Box::new(move |released| {
            Box::pin(async move { sent.lock().unwrap().push(("200", released)) })
        })
    };
    outboxes.push("c1", Some(wait), effects).await;
    settle().await;
    assert!(sent.lock().unwrap().is_empty());
    drop(serving);
    settle().await;
    assert_eq!(*sent.lock().unwrap(), vec![("200", Released::BackupLost)]);
    assert_eq!(metrics.repl_sync_capped_total(), 0, "a lost backup is no cap hit");
}

#[tokio::test(start_paused = true)]
async fn drained_resolves_once_the_last_entry_left() {
    let b = at(SyncLevel::Applied);
    let wait = b.write("c1");
    b.push("c1", Some(wait), "final").await;
    let outboxes = b.outboxes.clone();
    let drained = tokio::spawn(async move { outboxes.drained("c1").await });
    settle().await;
    assert!(!drained.is_finished(), "the entry is still queued");
    b.apply(1);
    tokio::time::timeout(Duration::from_millis(1), drained).await.expect("drained").unwrap();
    assert_eq!(b.sent(), vec![("final", Released::Applied)]);
}
