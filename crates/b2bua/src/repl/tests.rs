//! S4 storage-layer tests: changelog mutation/compaction/tombstone/TTL/auto-clean
//! + the `ReplicatingCallStore`'s peer/direction mapping and live-body drain.
//!
//! Under ADR-0014 the changelog is split into per-partition sub-logs (`Pri` =
//! Reclaim, `Bak` = Backup); a Forward put bumps `Bak`, a Reverse put bumps
//! `Pri`. `drain_since`/`peer_len`/`needs_reset` all take the partition, and the
//! poll server drains a bounded batch (here `NO_LIMIT` = "drain everything").

use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Op, Partition, Watermark};
use sip_clock::Clock;

use super::{Changelog, ReplicatingCallStore};
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const SELF: &str = "w0";
/// Forward puts land in the `Bak` sub-log (primary → backup).
const BAK_P: Partition = Partition::Bak;
/// "Drain everything" — the bounded poll batch never truncates these unit cases.
const NO_LIMIT: usize = usize::MAX;

fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

fn rev(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Reverse),
    }
}

async fn put(
    store: &ReplicatingCallStore,
    call_ref: &str,
    body: &[u8],
    ttl_ms: i64,
    call_gen: i64,
    opts: &PutOpts,
) {
    store
        .put_call(PRI, SELF, call_ref, body.to_vec(), &[], ttl_ms, call_gen, 0, opts)
        .await
        .unwrap();
}

/// Extract the single `Data` frame, asserting exactly one.
fn one_data(frames: Vec<Frame>) -> Frame {
    assert_eq!(frames.len(), 1, "expected one frame, got {:?}", frames);
    frames.into_iter().next().unwrap()
}

#[tokio::test(start_paused = true)]
async fn mutation_creates_entry_and_drains_live_body() {
    let store = ReplicatingCallStore::new(7, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;

    // changelog-for-A (Bak sub-log): one entry.
    assert_eq!(store.changelog().peer_len("A", BAK_P), 1);
    // head advanced from (7,0).
    assert_eq!(store.changelog().head(), Watermark::new(7, 1));

    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(7, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data {
            op,
            partition,
            call_ref,
            call_gen,
            body,
            ..
        } => {
            assert_eq!(op, Op::Put);
            assert_eq!(partition, Partition::Bak); // Forward → Bak
            assert_eq!(call_ref, "c1");
            assert_eq!(call_gen, 1);
            assert_eq!(body.as_deref(), Some(&b"v1"[..]));
        }
        f => panic!("not Data: {f:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn update_compacts_and_moves_counter_forward() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    let after_first = store.changelog().head(); // (1,1)
    put(&store, "c1", b"v2", 0, 2, &fwd("A")).await;

    // Compaction: still exactly one entry; counter moved to 2.
    assert_eq!(store.changelog().peer_len("A", BAK_P), 1);
    assert_eq!(store.changelog().head(), Watermark::new(1, 2));

    // Drain from the OLD watermark yields the latest body only, op=Put
    // (Create/Update merged — ADR-0014).
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, after_first, NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { op, body, call_gen, .. } => {
            assert_eq!(op, Op::Put);
            assert_eq!(call_gen, 2);
            assert_eq!(body.as_deref(), Some(&b"v2"[..]));
        }
        f => panic!("not Data: {f:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn compaction_under_churn_keeps_live_set_size() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    for i in 0..5 {
        put(&store, "X", format!("x{i}").as_bytes(), 0, i, &fwd("A")).await;
    }
    for i in 0..3 {
        put(&store, "Y", format!("y{i}").as_bytes(), 0, i, &fwd("A")).await;
    }
    // 8 bumps, 2 live refs → exactly 2 entries.
    assert_eq!(store.changelog().peer_len("A", BAK_P), 2);

    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    assert_eq!(frames.len(), 2);
    // Latest bodies, ascending by counter (Y was bumped last → higher counter).
    let mut bodies: Vec<Vec<u8>> = frames
        .iter()
        .map(|f| match f {
            Frame::Data { body, .. } => body.as_deref().unwrap().to_vec(),
            _ => panic!(),
        })
        .collect();
    bodies.sort();
    assert_eq!(bodies, vec![b"x4".to_vec(), b"y2".to_vec()]);
}

#[tokio::test(start_paused = true)]
async fn delete_emits_tombstone_then_reaped() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 60_000);
    let store = ReplicatingCallStore::with_changelog(cl, clock.clone());
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    store
        .delete_call(PRI, SELF, "c1", &[], &fwd("A"))
        .await
        .unwrap();

    // Tombstone present + drained as Delete with no body.
    assert_eq!(store.changelog().peer_len("A", BAK_P), 1);
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { op, body, .. } => {
            assert_eq!(op, Op::Delete);
            assert!(body.is_none());
        }
        f => panic!("not Data: {f:?}"),
    }

    // Advance past tombstone TTL + reap → gone.
    tokio::time::advance(Duration::from_millis(1_001)).await;
    store.changelog().reap(clock.now_ms());
    assert_eq!(store.changelog().peer_len("A", BAK_P), 0);
}

#[tokio::test(start_paused = true)]
async fn live_body_read_at_send_time() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    put(&store, "c1", b"v2", 0, 2, &fwd("A")).await;

    // Drain from genesis → must reflect v2 (read from store, not snapshotted).
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { body, .. } => assert_eq!(body.as_deref(), Some(&b"v2"[..])),
        f => panic!("not Data: {f:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn lock_discipline_concurrent_rewrite_is_consistent() {
    let store = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;

    let drainer = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .changelog()
                .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &*store, PRI, SELF)
                .await
        })
    };
    let writer = {
        let store = store.clone();
        tokio::spawn(async move {
            put(&store, "c1", b"v2", 0, 2, &fwd("A")).await;
        })
    };

    let (frames, _) = tokio::join!(drainer, writer);
    let frames = frames.unwrap();
    // No deadlock; a single consistent Arc (old or new), never torn.
    match one_data(frames) {
        Frame::Data { body, .. } => {
            let b = body.as_deref().unwrap();
            assert!(b == b"v1" || b == b"v2", "torn body: {b:?}");
        }
        f => panic!("not Data: {f:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn ttl_eviction_drops_body_on_access_and_reap() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());
    put(&store, "c1", b"v1", 500, 1, &fwd("A")).await;

    // Still live before TTL.
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_some());

    tokio::time::advance(Duration::from_millis(501)).await;
    // Lazy eviction on access.
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_none());

    // Explicit reap also clears any leftover meta.
    store.reap(clock.now_ms()).await;
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn dead_peer_auto_clean_via_drop_and_idle() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 2_000);
    let store = ReplicatingCallStore::with_changelog(cl, clock.clone());
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    assert!(store.changelog().has_peer("A"));

    // Explicit drop.
    assert!(store.changelog().drop_peer("A"));
    assert!(!store.changelog().has_peer("A"));

    // Re-create after drop starts clean from genesis.
    put(&store, "c2", b"w1", 0, 1, &fwd("A")).await;
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    assert_eq!(frames.len(), 1);

    // Idle-reap: advance past dead-peer TTL with no activity → peer dropped.
    put(&store, "c3", b"z1", 0, 1, &fwd("B")).await;
    tokio::time::advance(Duration::from_millis(2_001)).await;
    store.changelog().reap(clock.now_ms());
    assert!(!store.changelog().has_peer("B"));
}

#[tokio::test(start_paused = true)]
async fn reboot_incarnation_drains_all_for_lower_gen_watermark() {
    // Changelog built under gen=2; a puller still on gen=1 with a huge counter.
    let store = ReplicatingCallStore::new(2, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    put(&store, "c2", b"v2", 0, 1, &fwd("A")).await;

    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, u64::MAX), NO_LIMIT, &store, PRI, SELF)
        .await;
    // since.gen (1) < self.gen (2) → ALL live entries returned.
    assert_eq!(frames.len(), 2);
    for f in &frames {
        match f {
            Frame::Data { at, .. } => assert_eq!(at.gen, 2),
            _ => panic!(),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn multi_peer_changelogs_are_independent() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await;
    put(&store, "c2", b"v2", 0, 1, &fwd("B")).await;

    assert_eq!(store.changelog().peer_len("A", BAK_P), 1);
    assert_eq!(store.changelog().peer_len("B", BAK_P), 1);

    let a = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    let b = store
        .changelog()
        .drain_since("B", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    let cr = |f: &Frame| match f {
        Frame::Data { call_ref, .. } => call_ref.clone(),
        _ => panic!(),
    };
    assert_eq!(cr(&a[0]), "c1");
    assert_eq!(cr(&b[0]), "c2");
}

#[tokio::test(start_paused = true)]
async fn reverse_direction_maps_to_pri_partition() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &rev("A")).await;
    // Reverse → Pri sub-log.
    let frames = store
        .changelog()
        .drain_since("A", Partition::Pri, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { partition, .. } => assert_eq!(partition, Partition::Pri),
        f => panic!("not Data: {f:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn no_peer_stores_body_without_bump() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"v1", 0, 1, &PutOpts::default()).await;
    // Body stored, but no changelog entry for anyone.
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_some());
    assert!(!store.changelog().has_peer("A"));
    assert_eq!(store.changelog().head(), Watermark::new(1, 0));
}

// ---------------------------------------------------------------------------
// Review regressions: retention floor + ResetToBootstrap trigger (#1), TTL
// backstop for `ttl<=0` replicas (#2), serve-guard-aware reap (#7).
// ---------------------------------------------------------------------------

const BAK: PartitionRole = PartitionRole::Backup;

/// A reaped delete-tombstone raises the per-(peer,partition) retention floor, so a
/// warm puller whose `since` is below it is told to re-bootstrap (`needs_reset`),
/// while one at/above the floor — or on a cold (lower-gen) pull — is not.
#[tokio::test(start_paused = true)]
async fn needs_reset_after_tombstone_reap_raises_floor() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 60_000);
    let store = ReplicatingCallStore::with_changelog(cl.clone(), clock.clone());

    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await; // counter 1 (Bak)
    store.delete_call(PRI, SELF, "c1", &[], &fwd("A")).await.unwrap(); // counter 2 (tombstone)

    // Before the reap: a puller that saw up to counter 1 is NOT told to reset
    // (the tombstone at counter 2 is still live and would be re-delivered).
    assert!(!cl.needs_reset("A", BAK_P, Watermark::new(1, 1)), "tombstone still live → no reset");

    // Advance past the tombstone TTL and reap: the delete at counter 2 is gone,
    // the floor rises to 2.
    tokio::time::advance(Duration::from_millis(1_001)).await;
    cl.reap(clock.now_ms());

    assert!(cl.needs_reset("A", BAK_P, Watermark::new(1, 1)), "since below reaped tail → reset");
    assert!(!cl.needs_reset("A", BAK_P, Watermark::new(1, 2)), "since AT the floor → no reset");
    assert!(!cl.needs_reset("A", BAK_P, Watermark::new(0, 1)), "lower gen is a cold pull → no reset");
    assert!(!cl.needs_reset("Z", BAK_P, Watermark::new(1, 1)), "unknown peer → no reset");
}

/// A peer log with an active serve task (a held [`ServeGuard`]) is NOT reaped
/// while idle past the dead-peer TTL (so a parked poll-server never loses the log
/// it is draining out from under itself); once the guard drops, the next reap
/// evicts the now-unserved log.
///
/// [`ServeGuard`]: super::changelog::ServeGuard
#[tokio::test(start_paused = true)]
async fn served_peer_log_survives_reap_until_guard_dropped() {
    let clock = Clock::test_at(0);
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 2_000); // dead-peer TTL 2s
    let _store = ReplicatingCallStore::with_changelog(cl.clone(), clock.clone());

    let guard = cl.serving("A");
    assert!(cl.has_peer("A"));

    // Idle well past the dead-peer TTL, then reap: the served log survives.
    tokio::time::advance(Duration::from_millis(3_000)).await;
    cl.reap(clock.now_ms());
    assert!(cl.has_peer("A"), "a served peer log must survive an idle reap");

    // A bump refreshes the last-active stamp; drop the guard, idle past the TTL
    // again, and reap → the now-unserved log is evicted.
    cl.bump("A", "c1", Op::Put, Partition::Bak);
    drop(guard);
    tokio::time::advance(Duration::from_millis(3_000)).await;
    cl.reap(clock.now_ms());
    assert!(!cl.has_peer("A"), "an unserved idle log is reaped");
}

/// A replica stored with `ttl_ms <= 0` self-evicts via the backstop TTL (so a
/// missed delete cannot linger forever), instead of the old `expiry = None`.
#[tokio::test(start_paused = true)]
async fn nonpositive_ttl_replica_self_evicts_via_backstop() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone()).with_default_ttl_ms(1_000);

    // Apply-path replica (peer:None, ttl 0) — the shape a puller stores.
    store
        .put_call(BAK, "A", "c1", b"v1".to_vec(), &[], 0, 1, 0, &PutOpts::default())
        .await
        .unwrap();
    assert!(store.get_call(BAK, "A", "c1").await.unwrap().is_some());

    // Past the backstop → lazily evicted on access (no permanent ghost).
    tokio::time::advance(Duration::from_millis(1_001)).await;
    assert!(
        store.get_call(BAK, "A", "c1").await.unwrap().is_none(),
        "ttl<=0 replica self-evicts via the backstop"
    );
}
