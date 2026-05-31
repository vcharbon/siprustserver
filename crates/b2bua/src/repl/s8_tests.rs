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
use std::time::Duration;

use repl_net::transport::{Fault, ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::{
    flush_replicated, replication_target, Changelog, PullerConfig, ReplServer,
    ReplicatingCallStore, ReplicationPlan, ReplicationSupervisor,
};
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9800 + n))
}

/// Forward (primary→backup) put options targeting `peer`.
fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

/// The trivial 2-node backup resolver: the backup is always "the other node".
/// (S10 sources this from the proxy's `w_bak` cookie instead — see
/// [`replication_target`]'s doc-comment.)
fn resolver_to(peer: &'static str) -> impl Fn(&str) -> Option<String> {
    move |_call_ref: &str| Some(peer.to_string())
}

/// A node = its store + changelog + listen address; the server runs in the bg.
struct Node {
    store: ReplicatingCallStore,
    addr: SocketAddr,
}

impl Node {
    async fn spawn(
        ordinal: &str,
        addr: SocketAddr,
        gen: u64,
        net: &Arc<SimulatedReplicationNetwork>,
        clock: &Clock,
    ) -> Self {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(30_000, 300_000);
        let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
        let listener = net.listen(addr).await.unwrap();
        let server = ReplServer::new(ordinal, changelog, Arc::new(store.clone()));
        tokio::spawn(server.run(listener));
        Self { store, addr }
    }
}

fn supervisor_for(
    self_ordinal: &str,
    store: &ReplicatingCallStore,
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
    addrs: Vec<(String, SocketAddr)>,
) -> ReplicationSupervisor {
    let map: std::collections::HashMap<String, SocketAddr> = addrs.into_iter().collect();
    let resolve = Arc::new(move |peer: &Peer| *map.get(&peer.ordinal).unwrap());
    ReplicationSupervisor::with_config(
        self_ordinal,
        net.clone(),
        store.clone(),
        resolve,
        clock.clone(),
        fast_config(),
    )
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

async fn tick(ms: u64) {
    let chunks = ms.div_ceil(100).max(1);
    for _ in 0..chunks {
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
}

/// callRef whose encoded primary is `primary` (so partition_of / the policy
/// route it).
fn cref(primary: &str, id: &str) -> String {
    format!("{primary}|{id}|t{id}")
}

/// Membership of a single peer.
fn one_peer(ordinal: &str, clock: &Clock) -> Arc<SimulatedMembership> {
    Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new(ordinal, ordinal)],
        clock.clone(),
    ))
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
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
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
        1,
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

    // B takes over and MUTATES the call at call_gen=2 through the WRITE policy.
    // B does NOT own "A|.." → policy picks (A, Reverse) → store bak:A, propagate
    // Reverse → changelog-for-A bumps partition=Pri.
    let plan_b = flush_replicated(
        &b.store,
        "B",
        &c,
        b"v2-takeover-by-B".to_vec(),
        &[],
        0,
        2,
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
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(one_peer("B", &clock));
    tick(300).await;

    // THE HEADLINE ASSERTION: A's pri:A holds B's call_gen=2 body, NOT the stale
    // pre-crash call_gen=1.
    assert_eq!(
        a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"v2-takeover-by-B"[..]),
        "rebooted A reclaims the acting-backup's call_gen=2 mutation (LWW), not the stale gen=1"
    );
    assert_eq!(
        a2_store.current_call_gen(PRI, "A", &c),
        Some(2),
        "reclaimed at call_gen=2"
    );
    assert!(a2_sup.bootstrap_complete("B"), "A bootstrap-complete from B");
}

// ---------------------------------------------------------------------------
// callGen LWW ordering: a higher gen, then a stale lower gen for the same ref
// (out-of-order / reconnect replay) — the higher gen survives; the lower never
// overwrites; equal-gen re-delivery is a no-op.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn callgen_lww_ordering_high_wins_low_and_equal_noop() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");

    // A pushes call_gen=3 (the eventual winner).
    a.store
        .put_call(PRI, "A", &c, b"gen3".to_vec(), &[], 0, 3, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"gen3"[..]),
        "B holds gen3"
    );

    // A pushes a STALE call_gen=2 for the same ref (e.g. out-of-order replay).
    // The changelog bumps but B's LWW apply-gate skips the body write.
    a.store
        .put_call(PRI, "A", &c, b"gen2-stale".to_vec(), &[], 0, 2, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"gen3"[..]),
        "lower call_gen=2 must NOT overwrite the higher gen3"
    );

    // Equal-gen (3) re-delivery is a no-op: same body, no regression.
    a.store
        .put_call(PRI, "A", &c, b"gen3-again".to_vec(), &[], 0, 3, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"gen3"[..]),
        "equal call_gen re-delivery is a no-op (body write skipped)"
    );
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
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");
    // A forward-replicates call_gen=1.
    flush_replicated(&a.store, "A", &c, b"v1".to_vec(), &[], 0, 1, &resolver_to("B"))
        .await
        .unwrap();
    tick(100).await;

    // A is DOWN (its reclaiming incarnation has not started pulling yet). While A
    // is unreachable, B (acting-backup) takes over: mutate call_gen=2 via the
    // policy → Reverse → changelog-for-A partition=Pri.
    flush_replicated(&b.store, "B", &c, b"v2-while-A-down".to_vec(), &[], 0, 2, &resolver_to("A"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty under a higher incarnation gen and only NOW reconnects.
    // The cold re-pull from B delivers the takeover mutation it never saw live.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
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
    flush_replicated(&b.store, "B", &c, b"v3-during-cut".to_vec(), &[], 0, 3, &resolver_to("A"))
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
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(one_peer("A", &clock));
    let a_sup = supervisor_for("A", &a.store, &net, &clock, vec![("B".into(), b.addr)]);
    a_sup.start(one_peer("B", &clock));
    tick(100).await;

    let call_x = cref("A", "X"); // A's call → forward to B.
    let call_y = cref("B", "Y"); // B's call → forward to A.

    flush_replicated(&a.store, "A", &call_x, b"x1".to_vec(), &[], 0, 1, &resolver_to("B"))
        .await
        .unwrap();
    flush_replicated(&b.store, "B", &call_y, b"y1".to_vec(), &[], 0, 1, &resolver_to("A"))
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
    // (A does NOT own "B|.." → Reverse → propagate to B). call_gen=2.
    let plan = flush_replicated(&a.store, "A", &call_y, b"y2-takeover".to_vec(), &[], 0, 2, &resolver_to("B"))
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
    assert_eq!(b.store.current_call_gen(PRI, "B", &call_y), Some(2));
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
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(one_peer("A", &clock));
    tick(50).await;

    let c = cref("A", "1");
    flush_replicated(&a.store, "A", &c, b"g1".to_vec(), &[], 0, 1, &resolver_to("B"))
        .await
        .unwrap();
    tick(100).await;

    // B takes over twice (gen2 then gen3), reverse-propagating each.
    flush_replicated(&b.store, "B", &c, b"g2".to_vec(), &[], 0, 2, &resolver_to("A"))
        .await
        .unwrap();
    tick(50).await;
    flush_replicated(&b.store, "B", &c, b"g3".to_vec(), &[], 0, 3, &resolver_to("A"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty, reclaims, then quiesces.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(one_peer("B", &clock));
    tick(400).await;

    // Convergence invariant: both nodes agree on the body at the highest gen (3).
    let a_body = a2_store.get_call(PRI, "A", &c).await.unwrap();
    let b_body = b.store.get_call(BAK, "A", &c).await.unwrap();
    assert_eq!(a_body.as_deref(), Some(&b"g3"[..]), "A converged at gen3");
    assert_eq!(b_body.as_deref(), Some(&b"g3"[..]), "B holds gen3");
    assert_eq!(a_body, b_body, "A and B agree on the call body");
    assert_eq!(a2_store.current_call_gen(PRI, "A", &c), Some(3));
    assert_eq!(b.store.current_call_gen(BAK, "A", &c), Some(3));
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
