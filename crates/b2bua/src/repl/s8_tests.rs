//! S8 tests: the **reverse / takeover** replication path + the write-side policy
//! ([`replication_target`] / [`ReplicationPlan`] / [`flush_replicated`]).
//!
//! The headline shape: A is primary for a call, forward-replicates to B; A
//! crashes; B (acting-backup) MUTATES the call (higher `call_gen`) via the
//! write-side policy → the policy picks `Reverse` → the changelog bumps
//! `partition=Pri` for A; A reboots empty under a higher incarnation gen, tails
//! from B → reclaims the call as `pri:A` with B's NEWER body (LWW by `call_gen`).
//!
//! Unlike S6 (which hand-wrote `rev("A")` PutOpts), S8 drives every takeover
//! mutation through [`flush_replicated`] so the *policy* — not the test — chooses
//! the partition/peer/direction. That is the slice under test.
//!
//! All run under `#[tokio::test(start_paused = true)]`; the protocol is driven
//! BETWEEN `advance`s (advance to the deadline, let frames land, advance again)
//! per the CLAUDE.md fake-clock hazards. Transit delay is `>= 1 ms`.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::{Fault, SimulatedReplicationNetwork};
use sip_clock::Clock;

use super::test_support::{cref, fast_config, one_peer, rev, supervisor_for, tick, Node};
use super::{flush_replicated, replication_target, ReplicatingCallStore, ReplicationPlan};
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9800 + n))
}

/// The trivial 2-node backup resolver: the backup is always "the other node".
/// (S10 sources this from the proxy's `w_bak` cookie instead — see
/// [`replication_target`]'s doc-comment.)
fn resolver_to(peer: &'static str) -> impl Fn(&str) -> Option<String> {
    move |_call_ref: &str| Some(peer.to_string())
}

// ---------------------------------------------------------------------------
// HEADLINE: takeover-then-reclaim. A creates call_gen=1, forward-replicates to
// B. A crashes. B takes over via the acting-backup WRITE policy (call_gen=2) →
// reverse-propagates (changelog-for-A, partition=Pri). A reboots empty under a
// higher incarnation gen → tails from B → reclaims pri:A at call_gen=2 (NOT the
// stale call_gen=1).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn takeover_then_reclaim_keeps_backup_mutation() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    // A is primary; B backs A up (B pulls A → holds bak:A).
    let a = Node::spawn("A", addr(1), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(2), 1, &net, &clock).await;
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");

    // A (primary) creates the call at call_gen=1 and forward-replicates to B via
    // the WRITE policy: A owns the ref → policy picks (B, Forward) → bak:A on B.
    let plan_a = flush_replicated(
        &a.store,
        "A",
        &c,
        b"v1-from-A".to_vec(),
        &[],
        0,
        1, // p = 1 (primary create)
        0, // b = 0 (no takeover yet)
        &resolver_to("B"),
    )
    .await
    .unwrap();
    assert_eq!(
        plan_a,
        ReplicationPlan {
            role: PartitionRole::Primary,
            primary: "A".into(),
            target: Some(("B".into(), PropagateDirection::Forward)),
        },
        "primary path: store pri:A, forward to B"
    );
    tick(100).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"v1-from-A"[..]),
        "B holds A's call_gen=1 body in bak:A"
    );

    // A CRASHES: stop its supervisor's relevance (we just won't reboot the old
    // store; B is now acting-backup for the failed-over dialog).

    // B takes over and MUTATES the call through the WRITE policy. B does NOT own
    // "A|.." → policy picks (A, Reverse) → store bak:A, propagate Reverse →
    // changelog-for-A bumps partition=Pri. Under ADR-0014 B bumps only its OWN
    // counter: `(p,b)` goes 1,0 → 1,1 (p stays at A's branch point 1).
    let plan_b = flush_replicated(
        &b.store,
        "B",
        &c,
        b"v2-takeover-by-B".to_vec(),
        &[],
        0,
        1, // p = 1 (A's branch point, untouched by B)
        1, // b = 1 (B's first takeover mutation)
        &resolver_to("A"),
    )
    .await
    .unwrap();
    assert_eq!(
        plan_b,
        ReplicationPlan {
            role: PartitionRole::Backup,
            primary: "A".into(),
            target: Some(("A".into(), PropagateDirection::Reverse)),
        },
        "acting-backup path: store bak:A, reverse-propagate to A"
    );

    // A REBOOTS: brand-new EMPTY store under a HIGHER incarnation gen (2), cold.
    // A tails from B → bootstrap pre-seed + cold Replog deliver the Pri frame.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a2_sup.start(one_peer("B", &clock));
    tick(300).await;

    // THE HEADLINE ASSERTION: A's pri:A holds B's call_gen=2 body, NOT the stale
    // pre-crash call_gen=1.
    assert_eq!(
        a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"v2-takeover-by-B"[..]),
        "rebooted A reclaims the acting-backup's (1,1) mutation, not the stale (1,0)"
    );
    assert_eq!(
        a2_store.current_cv(PRI, "A", &c),
        Some((1, 1)),
        "reclaimed at version vector (p=1, b=1)"
    );
    assert!(a2_sup.bootstrap_complete("B"), "A bootstrap-complete from B");
}

// ---------------------------------------------------------------------------
// (p,b) REVERSE ordering (ADR-0014): the primary applies a backup reverse-flush
// iff it has not itself moved past the backup's branch point (`p_in == p_cur`)
// AND the backup genuinely advanced (`b_in > b_cur`). A higher `b` wins; a stale
// lower `b` never overwrites; an equal `(p,b)` re-delivery is a no-op. (This is
// the meaningful LWW now: a FORWARD primary→backup update always applies — the
// follower defers to authority — with the watermark apply-gate handling order.)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reverse_pb_high_b_wins_low_and_equal_noop() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;
    // A pulls B so A APPLIES B's reverse-flushes (partition=Pri for A's own ref).
    let a_sup = supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup.start(one_peer("B", &clock));
    tick(50).await;

    let c = cref("A", "1");

    // A holds its own pre-crash copy at (p=1, b=0).
    a.store
        .put_call(PRI, "A", &c, b"a-v1".to_vec(), &[], 0, 1, 0, &PutOpts::default())
        .await
        .unwrap();

    // B (acting-backup) reverse-flushes a genuine advance (1,2) — b jumped past
    // A's stored b=0 with p unchanged → A applies it.
    b.store
        .put_call(BAK, "A", &c, b"b2".to_vec(), &[], 0, 1, 2, &rev("A"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        a.store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"b2"[..]),
        "A applies the backup's genuine advance (1,2)"
    );

    // B reverse-flushes a STALE lower b (1,1): `b_in=1 > b_cur=2` is false → A
    // keeps its own (1,2). (Changelog still bumps; the (p,b) rule rejects it.)
    b.store
        .put_call(BAK, "A", &c, b"b1-stale".to_vec(), &[], 0, 1, 1, &rev("A"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        a.store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"b2"[..]),
        "a stale lower b must NOT overwrite the higher (1,2)"
    );

    // Equal (1,2) re-delivery is a no-op: `b_in > b_cur` is false.
    b.store
        .put_call(BAK, "A", &c, b"b2-again".to_vec(), &[], 0, 1, 2, &rev("A"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        a.store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"b2"[..]),
        "equal (1,2) re-delivery is a no-op"
    );
    assert_eq!(a.store.current_cv(PRI, "A", &c), Some((1, 2)));
}

// ---------------------------------------------------------------------------
// reverse during partition / unreachable primary: B's takeover mutation happens
// while A is unreachable (A is down — its puller not yet started). When A reboots
// and (re)connects, the cold re-pull delivers the takeover mutation. A mid-cut on
// the established stream + heal is exercised too, to prove the retained-W tail /
// cold re-pull both deliver it.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reverse_while_primary_unreachable_reclaimed_on_reconnect() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(21), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(22), 1, &net, &clock).await;
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");
    // A forward-replicates (p=1, b=0).
    flush_replicated(&a.store, "A", &c, b"v1".to_vec(), &[], 0, 1, 0, &resolver_to("B"))
        .await
        .unwrap();
    tick(100).await;

    // A is DOWN (its reclaiming incarnation has not started pulling yet). While A
    // is unreachable, B (acting-backup) takes over: mutate to (p=1, b=1) via the
    // policy → Reverse → changelog-for-A partition=Pri.
    flush_replicated(&b.store, "B", &c, b"v2-while-A-down".to_vec(), &[], 0, 1, 1, &resolver_to("A"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty under a higher incarnation gen and only NOW reconnects.
    // The cold re-pull from B delivers the takeover mutation it never saw live.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a2_sup.start(one_peer("B", &clock));
    tick(300).await;
    assert_eq!(
        a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"v2-while-A-down"[..]),
        "on reconnect A reclaims the takeover mutation made while it was unreachable"
    );

    // Now exercise a mid-stream cut + a further takeover during the cut + heal:
    // the retained-W tail must still deliver the newest body.
    net.apply_fault(Fault::Partition { a: b.addr, b: a.addr });
    tick(50).await;
    flush_replicated(&b.store, "B", &c, b"v3-during-cut".to_vec(), &[], 0, 1, 2, &resolver_to("A"))
        .await
        .unwrap();
    tick(50).await;
    net.apply_fault(Fault::Heal { a: b.addr, b: a.addr });
    tick(400).await;
    assert_eq!(
        a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"v3-during-cut"[..]),
        "after heal A reclaims the takeover mutation made during the partition"
    );
}

// ---------------------------------------------------------------------------
// bidirectional coexistence: A is primary for callX (B backs up) AND B is
// primary for callY (A backs up); both forward streams flow; then A takes over
// callY (B "crashed") — both directions converge independently (changelogs are
// per-peer, partitions independent).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn bidirectional_coexistence_converges_independently() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(31), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(32), 1, &net, &clock).await;

    // B pulls A (so B holds bak:A); A pulls B (so A holds bak:B). Both streams.
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    let a_sup = supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup.start(one_peer("B", &clock));
    tick(100).await;

    let call_x = cref("A", "X"); // A's call → forward to B.
    let call_y = cref("B", "Y"); // B's call → forward to A.

    flush_replicated(&a.store, "A", &call_x, b"x1".to_vec(), &[], 0, 1, 0, &resolver_to("B"))
        .await
        .unwrap();
    flush_replicated(&b.store, "B", &call_y, b"y1".to_vec(), &[], 0, 1, 0, &resolver_to("A"))
        .await
        .unwrap();
    tick(150).await;

    // Both forward streams landed: B holds bak:A/X, A holds bak:B/Y.
    assert_eq!(
        b.store.get_call(BAK, "A", &call_x).await.unwrap().as_deref(),
        Some(&b"x1"[..]),
        "B backs up A's callX"
    );
    assert_eq!(
        a.store.get_call(BAK, "B", &call_y).await.unwrap().as_deref(),
        Some(&b"y1"[..]),
        "A backs up B's callY"
    );

    // Now B "crashes" for callY: A takes over callY via the acting-backup policy
    // (A does NOT own "B|.." → Reverse → propagate to B). A bumps only b: (1,0)→(1,1).
    let plan = flush_replicated(&a.store, "A", &call_y, b"y2-takeover".to_vec(), &[], 0, 1, 1, &resolver_to("B"))
        .await
        .unwrap();
    assert_eq!(plan.role, PartitionRole::Backup);
    assert_eq!(plan.primary, "B");
    assert_eq!(plan.target, Some(("B".into(), PropagateDirection::Reverse)));
    tick(200).await;

    // callX still converges forward (B's bak:A unchanged at x1); callY's takeover
    // reverse-propagated to B's pri:B at gen2. Independent per-peer changelogs.
    assert_eq!(
        b.store.get_call(BAK, "A", &call_x).await.unwrap().as_deref(),
        Some(&b"x1"[..]),
        "callX forward stream unaffected"
    );
    assert_eq!(
        b.store.get_call(PRI, "B", &call_y).await.unwrap().as_deref(),
        Some(&b"y2-takeover"[..]),
        "callY takeover reverse-propagated to B's pri:B"
    );
    assert_eq!(b.store.current_cv(PRI, "B", &call_y), Some((1, 1)));
}

// ---------------------------------------------------------------------------
// convergence property: after a takeover + reboot + quiescence, A and B agree on
// the call at the highest call_gen (goal-1 convergence, scoped to 2 nodes).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn convergence_after_takeover_and_reboot_highest_gen() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(41), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(42), 1, &net, &clock).await;
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_config());
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");
    flush_replicated(&a.store, "A", &c, b"g1".to_vec(), &[], 0, 1, 0, &resolver_to("B"))
        .await
        .unwrap();
    tick(100).await;

    // B takes over twice, bumping only its own counter b: (1,0)→(1,1)→(1,2).
    flush_replicated(&b.store, "B", &c, b"g2".to_vec(), &[], 0, 1, 1, &resolver_to("A"))
        .await
        .unwrap();
    tick(50).await;
    flush_replicated(&b.store, "B", &c, b"g3".to_vec(), &[], 0, 1, 2, &resolver_to("A"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty, reclaims, then quiesces.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a2_sup.start(one_peer("B", &clock));
    tick(400).await;

    // Convergence invariant: both nodes agree on the body at the highest gen (3).
    let a_body = a2_store.get_call(PRI, "A", &c).await.unwrap();
    let b_body = b.store.get_call(BAK, "A", &c).await.unwrap();
    assert_eq!(a_body.as_deref(), Some(&b"g3"[..]), "A converged at (1,2)/g3");
    assert_eq!(b_body.as_deref(), Some(&b"g3"[..]), "B holds (1,2)/g3");
    assert_eq!(a_body, b_body, "A and B agree on the call body");
    assert_eq!(a2_store.current_cv(PRI, "A", &c), Some((1, 2)));
    assert_eq!(b.store.current_cv(BAK, "A", &c), Some((1, 2)));
    assert!(a2_sup.bootstrap_complete("B"));
}

// ---------------------------------------------------------------------------
// policy unit smoke at the integration layer: the reverse write maps
// Backup+Reverse → changelog partition=Pri (assert via the head-bump + a cold
// drain delivering a Pri frame is covered by the headline; here we assert the
// raw mapping shape through replication_target directly).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn policy_directions_at_integration_layer() {
    // Primary path → forward to backup.
    assert_eq!(
        replication_target("A", &cref("A", "1"), &resolver_to("B")),
        Some(("B".to_string(), PropagateDirection::Forward))
    );
    // Acting-backup path → reverse to the original primary.
    assert_eq!(
        replication_target("B", &cref("A", "1"), &resolver_to("A")),
        Some(("A".to_string(), PropagateDirection::Reverse))
    );
    // No backup resolvable on the primary path → None (local-only).
    let none = |_c: &str| None;
    assert_eq!(replication_target("A", &cref("A", "1"), &none), None);
}
