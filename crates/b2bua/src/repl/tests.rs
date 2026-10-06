//! Storage-layer tests: changelog mutation/compaction/tombstone/TTL/auto-clean
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

use super::{BodySource, Changelog, ReplicatingCallStore};
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
        ..PutOpts::default()
    }
}

fn rev(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Reverse),
        ..PutOpts::default()
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
        Frame::Data { op, partition, call_ref, call_gen, body, .. } => {
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
    let frames =
        store.changelog().drain_since("A", BAK_P, after_first, NO_LIMIT, &store, PRI, SELF).await;
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
    store.delete_call(PRI, SELF, "c1", &[], false, &fwd("A")).await.unwrap();

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

/// An expired body reads as absent, and a read leaves it in place: only the
/// reap evicts it, and hands it back once.
#[tokio::test(start_paused = true)]
async fn an_expired_body_reads_absent_and_only_the_reap_evicts_it() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());
    put(&store, "c1", b"v1", 500, 1, &fwd("A")).await;

    // Still live before TTL.
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_some());

    tokio::time::advance(Duration::from_millis(501)).await;
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_none(), "expired reads absent");
    assert!(store.peek_body_raw(PRI, SELF, "c1").await.is_some(), "the read evicted nothing");

    let evicted = store.reap(clock.now_ms()).await;
    assert_eq!(evicted.iter().map(|b| &b[..]).collect::<Vec<_>>(), vec![&b"v1"[..]]);
    assert!(store.peek_body_raw(PRI, SELF, "c1").await.is_none(), "the reap evicted it");
    assert!(store.reap(clock.now_ms()).await.is_empty(), "and hands it back once");
}

#[tokio::test(start_paused = true)]
async fn resurrection_tombstone_pruned_by_reap() {
    // The resurrection tombstone (apply-side delete-wins) must be PRUNED past its
    // window — `delete_call` inserts one per discharge and only `put_call` reads
    // it (within the window), so without a prune the map grows one entry per
    // terminated call forever (an unbounded leak). Behavioural probe: a Put for a
    // just-deleted ref is rejected (silent Ok, no body), then after the window
    // elapses + a reap the SAME ref accepts a Put again.
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone());

    put(&store, "c1", b"v1", 60_000, 1, &fwd("A")).await;
    store.delete_call(PRI, SELF, "c1", &[], false, &fwd("A")).await.unwrap();

    // Within the tombstone window: a re-creating Put is rejected (delete-wins).
    put(&store, "c1", b"v2", 60_000, 2, &fwd("A")).await;
    assert!(
        store.get_call(PRI, SELF, "c1").await.unwrap().is_none(),
        "Put within the resurrection window must be rejected"
    );

    // Past the window + a reap: the tombstone is pruned, so a fresh Put lands.
    tokio::time::advance(Duration::from_millis(300_001)).await;
    let _evicted = store.reap(clock.now_ms()).await;
    put(&store, "c1", b"v3", 60_000, 3, &fwd("A")).await;
    assert!(
        store.get_call(PRI, SELF, "c1").await.unwrap().is_some(),
        "after the window + reap the tombstone is gone and the Put is accepted"
    );
}

#[tokio::test(start_paused = true)]
async fn dead_peer_auto_clean_via_idle_reap() {
    // Idle reaping is the ONE dead-peer clean-up mechanism (no per-disconnect
    // drop — a disconnected puller's pending log is kept until the TTL so a
    // quick reconnect resumes warm).
    let clock = Clock::test_at(0);
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 2_000);
    let store = ReplicatingCallStore::with_changelog(cl, clock.clone());

    // Idle-reap: advance past dead-peer TTL with no activity → peer dropped.
    put(&store, "c3", b"z1", 0, 1, &fwd("B")).await;
    assert!(store.changelog().has_peer("B"));
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
// Retention floor + ResetToBootstrap trigger, TTL backstop for `ttl<=0`
// replicas, serve-guard-aware reap.
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
    store.delete_call(PRI, SELF, "c1", &[], false, &fwd("A")).await.unwrap(); // counter 2 (tombstone)

    // Before the reap: a puller that saw up to counter 1 is NOT told to reset
    // (the tombstone at counter 2 is still live and would be re-delivered).
    assert!(!cl.needs_reset("A", BAK_P, Watermark::new(1, 1)), "tombstone still live → no reset");

    // Advance past the tombstone TTL and reap: the delete at counter 2 is gone,
    // the floor rises to 2.
    tokio::time::advance(Duration::from_millis(1_001)).await;
    cl.reap(clock.now_ms());

    assert!(cl.needs_reset("A", BAK_P, Watermark::new(1, 1)), "since below reaped tail → reset");
    assert!(!cl.needs_reset("A", BAK_P, Watermark::new(1, 2)), "since AT the floor → no reset");
    assert!(
        !cl.needs_reset("A", BAK_P, Watermark::new(0, 1)),
        "lower gen is a cold pull → no reset"
    );
    assert!(!cl.needs_reset("Z", BAK_P, Watermark::new(1, 1)), "unknown peer → no reset");
}

/// An incarnation-gen collision or backward clock step must force a
/// `ResetToBootstrap` — never a silent warm tail. A sub-second crash-restart
/// (or an NTP step-back) can hand a fresh changelog the SAME (or a lower) gen
/// while its counter restarts at 0; a warm puller then presents a same-gen
/// watermark ABOVE our head (a counter we never issued) or a future-gen
/// watermark, and a `since.gen < gen ⇒ cold, else warm` split would silently
/// skip every new entry until the counter outgrew the stale watermark.
#[tokio::test(start_paused = true)]
async fn gen_collision_or_backward_clock_forces_reset() {
    let clock = Clock::test_at(0);
    // "Restarted" changelog: same gen 1 as the previous life, counter back at 0.
    let cl = Changelog::new(1, clock.clone()).with_ttls(1_000, 60_000);
    let store = ReplicatingCallStore::with_changelog(cl.clone(), clock.clone());
    put(&store, "c1", b"v1", 0, 1, &fwd("A")).await; // head = (1, 1)

    // Same-gen watermark above our head: the previous life issued it, we never
    // did — gen collision. Must reset, not warm-tail from counter 5000.
    assert!(
        cl.needs_reset("A", BAK_P, Watermark::new(1, 5_000)),
        "same-gen counter above head = collision → reset"
    );
    // Future-gen watermark: our boot clock stepped backward across the restart.
    assert!(cl.needs_reset("A", BAK_P, Watermark::new(2, 3)), "future-gen watermark → reset");
    // A legitimate same-gen watermark at/below head stays warm.
    assert!(
        !cl.needs_reset("A", BAK_P, Watermark::new(1, 1)),
        "watermark at head is an ordinary warm resume"
    );
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

    let guard = cl.serving("A", Partition::Bak);
    assert!(cl.has_peer("A"));

    // Idle well past the dead-peer TTL, then reap: the served log survives.
    tokio::time::advance(Duration::from_millis(3_000)).await;
    cl.reap(clock.now_ms());
    assert!(cl.has_peer("A"), "a served peer log must survive an idle reap");

    // A bump refreshes the last-active stamp; drop the guard, idle past the TTL
    // again, and reap → the now-unserved log is evicted.
    cl.bump_put("A", "c1", Partition::Bak);
    drop(guard);
    tokio::time::advance(Duration::from_millis(3_000)).await;
    cl.reap(clock.now_ms());
    assert!(!cl.has_peer("A"), "an unserved idle log is reaped");
}

/// A replica stored with `ttl_ms <= 0` self-evicts via the backstop TTL (so a
/// missed delete cannot linger forever), never `expiry = None`.
#[tokio::test(start_paused = true)]
async fn nonpositive_ttl_replica_self_evicts_via_backstop() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone()).with_default_ttl_ms(1_000);

    // Apply-path replica (peer:None, ttl 0) — the shape a puller stores. The
    // index keys ride along exactly as the puller passes them.
    let idx = vec!["a:cid-1|tag-a".to_string()];
    store
        .put_call(BAK, "A", "c1", b"v1".to_vec(), &idx, 0, 1, 0, &PutOpts::default())
        .await
        .unwrap();
    assert!(store.get_call(BAK, "A", "c1").await.unwrap().is_some());
    assert_eq!(store.get_index("a:cid-1|tag-a").await.unwrap().as_deref(), Some("c1"));

    // Past the backstop → reads absent (no permanent ghost), its idx:* entries
    // too — an index resolving to an expired body would let
    // `resolve_from_replica_index` resolve a takeover to a dead callRef.
    tokio::time::advance(Duration::from_millis(1_001)).await;
    assert!(
        store.get_call(BAK, "A", "c1").await.unwrap().is_none(),
        "ttl<=0 replica expires via the backstop"
    );
    assert_eq!(
        store.get_index("a:cid-1|tag-a").await.unwrap(),
        None,
        "an expired replica's index entries read absent too"
    );
}

/// The `reap` frees an expired ghost's `idx:*` entries along with its body, on
/// the sweep the core drives.
#[tokio::test(start_paused = true)]
async fn reap_frees_expired_replica_index_entries() {
    let clock = Clock::test_at(0);
    let store = ReplicatingCallStore::new(1, clock.clone()).with_default_ttl_ms(1_000);

    let idx = vec!["b:cid-2|tag-b".to_string(), "b:cid-2".to_string()];
    store
        .put_call(BAK, "A", "c2", b"v1".to_vec(), &idx, 0, 1, 0, &PutOpts::default())
        .await
        .unwrap();
    assert_eq!(store.get_index("b:cid-2|tag-b").await.unwrap().as_deref(), Some("c2"));

    tokio::time::advance(Duration::from_millis(1_001)).await;
    assert_eq!(store.reap(clock.now_ms()).await.len(), 1);

    assert!(store.get_call(BAK, "A", "c2").await.unwrap().is_none());
    assert_eq!(store.get_index("b:cid-2|tag-b").await.unwrap(), None);
    assert_eq!(store.get_index("b:cid-2").await.unwrap(), None);
}

// ---------------------------------------------------------------------------
// Per-ref metadata across writers: the record is keyed by callRef alone and
// every local writer rewrites it. A write carries over each field it does not
// itself carry (`backup`, `skew_offset_ms`, `authority_answered`) and replaces
// the ones it does (`(p,b)`, expiry, role/primary, the index set — every writer
// computes the full index set from the body, so `indexes` is carried by the
// write, not lost). Nothing here is scheduled: the store is synchronous under
// one mutex, so the order of the writes IS the order of the calls.
// ---------------------------------------------------------------------------

/// The puller's apply: a local write stamped with the origin node's wall clock
/// (`peer: None`, so nothing propagates).
fn applied(origin_now_ms: i64) -> PutOpts {
    PutOpts { origin_now_ms: Some(origin_now_ms), ..PutOpts::default() }
}

/// The four kinds of local write, each named for the failing assertion.
fn every_writer() -> Vec<(&'static str, PutOpts)> {
    vec![
        ("peerless", PutOpts::default()),
        ("reverse", rev(SELF)),
        ("forward", fwd("w1")),
        ("apply", applied(70_000)),
    ]
}

/// `every_writer` without the one that carries the field under test.
fn every_writer_but(carrier: &str) -> Vec<(&'static str, PutOpts)> {
    every_writer().into_iter().filter(|(w, _)| *w != carrier).collect()
}

/// The writers among `writers` — the writers that carry no value for `field`,
/// named as in `every_writer` — that lose it: each is run on a fresh store
/// seeded by one put under `seed_opts` then `after_seed`, and `holds` reads
/// the field back.
async fn writers_losing(
    field: &str,
    writers: Vec<(&'static str, PutOpts)>,
    seed_opts: &PutOpts,
    after_seed: impl Fn(&ReplicatingCallStore),
    holds: impl Fn(&ReplicatingCallStore) -> bool,
) -> Vec<&'static str> {
    let mut lost = Vec::new();
    for (writer, opts) in writers {
        let store = ReplicatingCallStore::new(1, Clock::test_at(100_000));
        put(&store, "c1", b"v1", 0, 1, seed_opts).await;
        after_seed(&store);
        assert!(holds(&store), "{field} not seeded before {writer}");
        put(&store, "c1", b"v2", 0, 2, &opts).await;
        if !holds(&store) {
            lost.push(writer);
        }
    }
    lost
}

/// The backup ordinal is carried only by a Forward write: every write that
/// carries none keeps it, and a Forward write to another backup replaces it.
/// The reverse write stays in the loop although a `bak:` record never holds a
/// backup ordinal: the store's carry-over is role-agnostic.
#[tokio::test(start_paused = true)]
async fn backup_ordinal_survives_every_write_that_does_not_carry_one() {
    let backed_by_w1 =
        |store: &ReplicatingCallStore| store.scan_refs_backed_by(SELF, "w1") == ["c1".to_string()];
    let lost =
        writers_losing("backup", every_writer_but("forward"), &fwd("w1"), |_| {}, backed_by_w1)
            .await;
    assert!(lost.is_empty(), "backup lost by {lost:?}");

    let store = ReplicatingCallStore::new(1, Clock::test_at(100_000));
    put(&store, "c1", b"v1", 0, 1, &fwd("w1")).await;
    put(&store, "c1", b"v3", 0, 3, &fwd("w2")).await;
    assert!(store.scan_refs_backed_by(SELF, "w1").is_empty(), "backup frozen across a forward");
    assert_eq!(store.scan_refs_backed_by(SELF, "w2"), vec!["c1".to_string()]);
}

/// The skew offset is carried only by an origin-stamped write (the puller's
/// apply): every local flush keeps it, and a later apply replaces it.
#[tokio::test(start_paused = true)]
async fn skew_offset_survives_every_write_that_does_not_carry_one() {
    let lost = writers_losing(
        "skew offset",
        every_writer_but("apply"),
        &applied(70_000),
        |_| {},
        |store| store.skew_offset_ms("c1") == Some(30_000),
    )
    .await;
    assert!(lost.is_empty(), "skew offset lost by {lost:?}");

    let store = ReplicatingCallStore::new(1, Clock::test_at(100_000));
    put(&store, "c1", b"v1", 0, 1, &applied(70_000)).await;
    put(&store, "c1", b"v3", 0, 3, &applied(60_000)).await;
    assert_eq!(store.skew_offset_ms("c1"), Some(40_000), "skew offset frozen across an apply");
}

/// The authority's answer is carried by no write at all: every kind of local
/// write and `reestablish_backup` keep it, and only the ref's own delete clears
/// it — a fresh record for the same ref starts unanswered.
#[tokio::test(start_paused = true)]
async fn authority_answer_survives_every_write_and_leaves_with_the_ref() {
    let lost = writers_losing(
        "authority answer",
        every_writer(),
        &PutOpts::default(),
        |store| {
            assert!(!store.authority_answered("c1"), "unanswered until the authority publishes");
            store.note_authority_answer("c1");
        },
        |store| store.authority_answered("c1"),
    )
    .await;
    assert!(lost.is_empty(), "authority answer lost by {lost:?}");

    let clock = Clock::test_at(100_000);
    let store = ReplicatingCallStore::new(1, clock.clone());
    put(&store, "c1", b"v1", 0, 1, &PutOpts::default()).await;
    store.note_authority_answer("c1");
    store.reestablish_backup("c1", "w1");
    assert!(store.authority_answered("c1"), "authority answer lost by reestablish_backup");

    store.delete_call(PRI, SELF, "c1", &[], false, &PutOpts::default()).await.unwrap();
    assert!(!store.authority_answered("c1"), "the answer leaves with the ref");

    // Past the resurrection tombstone, a fresh record under the same ref stands
    // and starts unanswered: the mark belonged to the record, not the ref.
    tokio::time::advance(Duration::from_millis(300_001)).await;
    let _evicted = store.reap(clock.now_ms()).await;
    put(&store, "c1", b"v3", 0, 3, &PutOpts::default()).await;
    assert_eq!(store.current_cv(PRI, SELF, "c1"), Some((3, 0)), "the fresh put stands");
    assert!(!store.authority_answered("c1"), "a fresh record starts unanswered");
}

/// The fields a write does carry are replaced by every writer: the `(p,b)`
/// version reads the new value after each write, and the expiry is refreshed
/// (the body outlives the TTL of the write before, inside the last one's).
#[tokio::test(start_paused = true)]
async fn carried_fields_are_replaced_by_every_write() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(100_000));
    store.put_call(PRI, SELF, "c1", b"v0".to_vec(), &[], 500, 1, 0, &fwd("w1")).await.unwrap();
    assert_eq!(store.current_cv(PRI, SELF, "c1"), Some((1, 0)));

    let mut previous = "seed";
    for (i, (writer, opts)) in every_writer().into_iter().enumerate() {
        // 300 ms on: inside the previous write's TTL, past the TTL of the one
        // before it — so from the second write on, a body still here proves
        // the previous write refreshed the expiry. The first check would sit
        // inside the seed's own TTL and prove nothing, so it is skipped.
        tokio::time::advance(Duration::from_millis(300)).await;
        if i > 0 {
            assert!(
                store.get_call(PRI, SELF, "c1").await.unwrap().is_some(),
                "expiry not refreshed by {previous}"
            );
        }
        let (p, b) = (10 + i as i64, 1 + i as i64);
        store.put_call(PRI, SELF, "c1", b"v".to_vec(), &[], 500, p, b, &opts).await.unwrap();
        assert_eq!(
            store.current_cv(PRI, SELF, "c1"),
            Some((p, b)),
            "version not replaced by {writer}"
        );
        previous = writer;
    }
    // 600 ms after the last write: past the write before it, inside its own TTL.
    tokio::time::advance(Duration::from_millis(300)).await;
    assert!(
        store.get_call(PRI, SELF, "c1").await.unwrap().is_some(),
        "expiry not refreshed by {previous}"
    );
    tokio::time::advance(Duration::from_millis(300)).await;
    assert!(
        store.get_call(PRI, SELF, "c1").await.unwrap().is_none(),
        "the refreshed TTL still expires"
    );
}

/// `opts` naming the call incarnation `incarnation`.
fn named(opts: PutOpts, incarnation: &str) -> PutOpts {
    PutOpts { incarnation: Some(incarnation.to_string()), ..opts }
}

/// The resurrection tombstone buries the call the delete removed, not its
/// callRef. Inside the window a late `Put` of that call — named or not — is
/// ignored, while a new call born on the same ref is stored and replicated at
/// once, under its own name; the buried call stays buried after it.
#[tokio::test(start_paused = true)]
async fn a_delete_buries_its_call_and_not_the_next_one_on_its_ref() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    put(&store, "c1", b"first", 60_000, 4, &named(fwd("A"), "c1#1")).await;
    // The delete names no call: the body it removes does.
    store.delete_call(PRI, SELF, "c1", &[], false, &fwd("A")).await.unwrap();

    put(&store, "c1", b"first-late", 60_000, 5, &named(rev("A"), "c1#1")).await;
    put(&store, "c1", b"unnamed-late", 60_000, 5, &PutOpts::default()).await;
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_none(), "the call stays buried");
    assert!(store.buries("c1", Some("c1#1")));
    assert!(!store.buries("c1", Some("c1#2")));

    put(&store, "c1", b"retry", 60_000, 1, &named(fwd("A"), "c1#2")).await;
    assert_eq!(store.get_call(PRI, SELF, "c1").await.unwrap().as_deref(), Some(&b"retry"[..]));
    assert_eq!(store.incarnation("c1").as_deref(), Some("c1#2"));
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { op, call_gen, body, incarnation, .. } => {
            assert_eq!(op, Op::Put, "the new call's put replicates");
            assert_eq!(call_gen, 1);
            assert_eq!(body.as_deref(), Some(&b"retry"[..]));
            assert_eq!(incarnation.as_deref(), Some("c1#2"), "under its own name");
        }
        other => panic!("expected Data, got {other:?}"),
    }

    put(&store, "c1", b"first-later", 60_000, 9, &named(rev("A"), "c1#1")).await;
    assert_eq!(
        store.get_call(PRI, SELF, "c1").await.unwrap().as_deref(),
        Some(&b"retry"[..]),
        "the buried call does not overwrite the new one"
    );
}

/// A delete removing no body buries the call it names, so a node that
/// never held the call still refuses it.
#[tokio::test(start_paused = true)]
async fn a_delete_naming_its_call_buries_it_with_no_body_held() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    store.delete_call(PRI, SELF, "c1", &[], false, &named(fwd("A"), "c1#1")).await.unwrap();
    put(&store, "c1", b"late", 60_000, 2, &named(rev("A"), "c1#1")).await;
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_none());
    put(&store, "c1", b"retry", 60_000, 1, &named(fwd("A"), "c1#2")).await;
    assert!(store.get_call(PRI, SELF, "c1").await.unwrap().is_some());
}

/// A write of another call on the ref starts a fresh record: the facts the
/// record kept about the call it replaces (the authority's answer, the backup
/// ordinal, the skew offset) are not the new call's. An unnamed write is one
/// of the held call and keeps its name.
#[tokio::test(start_paused = true)]
async fn a_write_of_another_call_on_the_ref_starts_a_fresh_record() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(100_000));
    put(&store, "c1", b"first", 0, 3, &named(fwd("w1"), "c1#1")).await;
    put(&store, "c1", b"first", 0, 3, &named(applied(60_000), "c1#1")).await;
    store.note_authority_answer("c1");
    put(&store, "c1", b"first", 0, 4, &PutOpts::default()).await;
    assert_eq!(store.incarnation("c1").as_deref(), Some("c1#1"), "an unnamed write keeps it");
    assert!(store.authority_answered("c1"));
    assert_eq!(store.skew_offset_ms("c1"), Some(40_000));
    assert_eq!(store.scan_refs_backed_by(SELF, "w1"), vec!["c1".to_string()]);

    put(&store, "c1", b"retry", 0, 1, &named(PutOpts::default(), "c1#2")).await;
    assert_eq!(store.incarnation("c1").as_deref(), Some("c1#2"));
    assert_eq!(store.current_cv(PRI, SELF, "c1"), Some((1, 0)));
    assert!(!store.authority_answered("c1"), "the new call starts unanswered");
    assert_eq!(store.skew_offset_ms("c1"), None, "and with no skew offset");
    assert!(store.scan_refs_backed_by(SELF, "w1").is_empty(), "and with no backup ordinal");
}

/// A `Delete` frame names the call the delete removed — the one the delete
/// named, or the body it found — so its receiver buries that call and no
/// other.
#[tokio::test(start_paused = true)]
async fn a_delete_frame_names_the_call_it_removed() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    for (named_by_delete, expected) in [(true, "c1#1"), (false, "c1#1")] {
        let fresh = ReplicatingCallStore::new(1, Clock::test_at(0));
        put(&fresh, "c1", b"first", 60_000, 1, &named(fwd("A"), "c1#1")).await;
        let opts = if named_by_delete { named(fwd("A"), "c1#1") } else { fwd("A") };
        fresh.delete_call(PRI, SELF, "c1", &[], false, &opts).await.unwrap();
        let frames = fresh
            .changelog()
            .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &fresh, PRI, SELF)
            .await;
        match one_data(frames) {
            Frame::Data { op, incarnation, .. } => {
                assert_eq!(op, Op::Delete);
                assert_eq!(incarnation.as_deref(), Some(expected), "named: {named_by_delete}");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }
    // A delete of another call than the held one leaves the held call, buries
    // the named one, and propagates nothing over the held call's entry.
    put(&store, "c1", b"retry", 60_000, 1, &named(fwd("A"), "c1#2")).await;
    store.delete_call(PRI, SELF, "c1", &[], false, &named(fwd("A"), "c1#1")).await.unwrap();
    assert_eq!(store.get_call(PRI, SELF, "c1").await.unwrap().as_deref(), Some(&b"retry"[..]));
    assert!(store.buries("c1", Some("c1#1")));
    let frames = store
        .changelog()
        .drain_since("A", BAK_P, Watermark::new(1, 0), NO_LIMIT, &store, PRI, SELF)
        .await;
    match one_data(frames) {
        Frame::Data { op, incarnation, .. } => {
            assert_eq!((op, incarnation.as_deref()), (Op::Put, Some("c1#2")), "the retry's Put");
        }
        other => panic!("expected Data, got {other:?}"),
    }
}

/// A write of another call that replaces the held one takes the replaced
/// call's index keys with it, even when the write itself carries none.
#[tokio::test(start_paused = true)]
async fn a_write_of_another_call_drops_the_replaced_calls_index_keys() {
    let store = ReplicatingCallStore::new(1, Clock::test_at(0));
    let first = vec!["leg:first|a".to_string()];
    store
        .put_call(PRI, SELF, "c1", b"first".to_vec(), &first, 0, 3, 0, &named(fwd("A"), "c1#1"))
        .await
        .unwrap();
    assert_eq!(store.get_index("leg:first|a").await.unwrap().as_deref(), Some("c1"));
    store
        .put_call(PRI, SELF, "c1", b"retry".to_vec(), &[], 0, 1, 0, &named(fwd("A"), "c1#2"))
        .await
        .unwrap();
    assert_eq!(store.get_index("leg:first|a").await.unwrap(), None, "the replaced call's key");
    assert_eq!(store.map_lens().1, 0, "no index key stands for the replaced call");
}
