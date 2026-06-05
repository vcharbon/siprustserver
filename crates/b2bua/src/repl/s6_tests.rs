//! S6 tests: Bootstrap / re-hydration — the lazy-batch `bak:{caller}` scan, the
//! single-connection Bootstrap→Replog handoff, and the hard-timer backstop.
//!
//! Like S5, all run under `#[tokio::test(start_paused = true)]`; the protocol is
//! driven BETWEEN `advance`s (advance to the deadline, let frames land, advance
//! again) per the CLAUDE.md fake-clock hazards. Transit delay is `>= 1 ms`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::Watermark;
use repl_net::transport::{Fault, ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::{Changelog, FnPeerResolver, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor};
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

/// Short backoff + short bootstrap hard timeout so a couple of advances trip
/// the relevant deadline deterministically.
fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9600 + n))
}

/// Forward (primary→backup) put options targeting `peer`.
fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

/// Reverse (acting-backup→reclaiming-primary) put options targeting `peer`.
fn rev(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Reverse),
    }
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

/// Wire a supervisor to pull from a set of peers.
fn supervisor_for(
    self_ordinal: &str,
    store: &ReplicatingCallStore,
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
    addrs: Vec<(String, SocketAddr)>,
) -> ReplicationSupervisor {
    let map: std::collections::HashMap<String, SocketAddr> = addrs.into_iter().collect();
    let resolve = Arc::new(FnPeerResolver(move |peer: &Peer| *map.get(&peer.ordinal).unwrap()));
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

/// callRef whose encoded primary is `primary` (so partition_of routes it).
fn cref(primary: &str, id: &str) -> String {
    format!("{primary}|{id}|t{id}")
}

// ---------------------------------------------------------------------------
// reboot recovery (the headline): A forward-replicates 3 calls to B (bak:A);
// A reboots empty under a higher gen, cold; A bootstraps from B → gets them as
// pri:A; the tail keeps A current.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reboot_recovery_reclaims_pri_partition() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    // A is the primary; B backs A up. A forward-replicates 3 calls to B.
    let a = Node::spawn("A", addr(1), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(2), 1, &net, &clock).await;

    // B pulls A (so it holds A's calls in bak:A).
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    let bodies = ["body0", "body1", "body2"];
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        a.store
            .put_call(PRI, "A", &c, body.as_bytes().to_vec(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }
    tick(100).await;
    // Sanity: B holds all 3 in bak:A.
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        assert_eq!(
            b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
            Some(body.as_bytes()),
            "B holds A's call {i} in bak:A"
        );
    }

    // Simulate A reboot: brand-new EMPTY store under a HIGHER incarnation gen,
    // cold. A now bootstraps from B (pulling B's bak:A partition as pri:A).
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    tick(200).await;

    // THE HEADLINE ASSERTION: A's store has all 3 as pri:A with the originals.
    for (i, body) in bodies.iter().enumerate() {
        let c = cref("A", &i.to_string());
        assert_eq!(
            a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
            Some(body.as_bytes()),
            "rebooted A reclaims call {i} as pri:A via bootstrap"
        );
    }
    assert!(a2_sup.bootstrap_complete("B"), "A bootstrap-complete from B");
    assert!(a2_sup.all_bootstrapped(), "A fully bootstrapped");

    // The tail keeps A current: B reverse-mutates a call → A picks it up.
    let c0 = cref("A", "0");
    b.store
        // Reverse/takeover mutation bumps the BACKUP counter b (p stays at A's
        // branch point 1): (1,0) → (1,1). p_in==sp & b_in>sb ⇒ A applies.
        .put_call(BAK, "A", &c0, b"tail-updated".to_vec(), &[], 0, 1, 1, &rev("A"))
        .await
        .unwrap();
    tick(200).await;
    assert_eq!(
        a2_store.get_call(PRI, "A", &c0).await.unwrap().as_deref(),
        Some(&b"tail-updated"[..]),
        "post-bootstrap tail keeps A current"
    );
}

// ---------------------------------------------------------------------------
// concurrent mutation during scan: while A bootstraps, B reverse-mutates one of
// A's calls (higher call_gen). A must end with the NEWEST body (seed-W + tail
// re-delivers it; idempotent by call_gen — not clobbered by the older copy).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn concurrent_mutation_during_scan_keeps_newest() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;

    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    // A forward-replicates 1 call (call_gen 1) to B.
    let c = cref("A", "0");
    a.store
        .put_call(PRI, "A", &c, b"v1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty; bootstrap from B begins. BEFORE the tail catches up,
    // B (acting-backup) reverse-mutates the call bumping the BACKUP counter b
    // (p stays at A's branch point 1): (1,0) → (1,1), which bumps changelog-for-A
    // (partition=Pri).
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    // Let bootstrap exchange begin / complete the pre-seed.
    tick(50).await;
    // Concurrent reverse mutation on B at (1,1).
    b.store
        .put_call(BAK, "A", &c, b"v2-newer".to_vec(), &[], 0, 1, 1, &rev("A"))
        .await
        .unwrap();
    tick(300).await;

    // A converges on the NEWEST body (v2), not the older bootstrap copy — the
    // (p,b) reverse rule (p unchanged, b advanced) plus tail re-delivery.
    assert_eq!(
        a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"v2-newer"[..]),
        "A ends with the newest body; (p,b) reverse rule, tail re-delivers"
    );
}

// ---------------------------------------------------------------------------
// terminal handoff: after the terminal Noop the puller sends Replog(since=W) on
// the SAME connection and picks up a post-scan mutation (counter>W).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn terminal_handoff_switches_to_replog_same_connection() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(21), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(22), 1, &net, &clock).await;

    // B holds one of A's calls in bak:A.
    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;
    let c0 = cref("A", "0");
    a.store
        .put_call(PRI, "A", &c0, b"seed".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;

    // A reboots empty; bootstrap then tail (single connection).
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    tick(150).await;
    assert!(a2_sup.bootstrap_complete("B"), "terminal Noop seen");
    let w_seed = a2_sup.watermark("B");

    // A POST-SCAN mutation on B (counter advances beyond the seed W). Because the
    // same connection switched to Replog(since=W), the tail must deliver it.
    let c1 = cref("A", "1");
    b.store
        .put_call(BAK, "A", &c1, b"post-scan".to_vec(), &[], 0, 1, 0, &rev("A"))
        .await
        .unwrap();
    tick(200).await;

    assert_eq!(
        a2_store.get_call(PRI, "A", &c1).await.unwrap().as_deref(),
        Some(&b"post-scan"[..]),
        "post-scan mutation delivered via the Replog tail on the same connection"
    );
    assert!(
        a2_sup.watermark("B") > w_seed,
        "watermark advanced past the bootstrap seed via the tail"
    );
}

// ---------------------------------------------------------------------------
// hard timer — unreachable peer: A boots with B partitioned (connect fails).
// Past the hard timeout, all_bootstrapped() becomes true (A serves) despite
// never reaching B.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn hard_timer_unreachable_peer_boots_anyway() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a_addr = addr(31);
    let b_addr = addr(32);

    // A has a store + supervisor pulling B, but B is partitioned from A — every
    // connect is refused/blocked, so bootstrap can never reach a terminal Noop.
    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_listener = net.listen(a_addr).await.unwrap();
    tokio::spawn(
        ReplServer::new("A", Changelog::new(1, clock.clone()), Arc::new(a_store.clone()))
            .run(a_listener),
    );
    // Cut A→B so connect is blocked.
    net.apply_fault(Fault::Partition { a: a_addr, b: b_addr });

    let a_sup = supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b_addr)]);
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));

    // Before the hard timeout: not yet bootstrap-complete (connect keeps failing).
    tick(200).await;
    assert!(
        !a_sup.all_bootstrapped(),
        "still trying to reach B before the hard timeout"
    );

    // Advance past the 2s bootstrap hard timeout → best-effort complete.
    tick(2_500).await;
    assert!(
        a_sup.bootstrap_complete("B"),
        "hard timer marks B bootstrap-complete best-effort"
    );
    assert!(a_sup.all_bootstrapped(), "A boots and serves despite unreachable B");
}

// ---------------------------------------------------------------------------
// hard timer — stalled bootstrap: A connects but B stalls mid-scan (Stall fault
// on the B→A direction). Past the timeout A marks complete best-effort.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn hard_timer_stalled_bootstrap_completes_best_effort() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(42);

    // A STALLING peer at B: it accepts the connection and reads the PullRequest
    // but NEVER replies — no Data, no terminal Noop. (We cannot target the
    // simulated Stall fault at a puller's connection because `connect`
    // synthesises an ephemeral client-side local addr, so the directed pair key
    // is unknown; an inline never-replying server is the faithful stall.) A's
    // bootstrap recv loop must trip the hard timer.
    let stall_listener = net.listen(b_addr).await.unwrap();
    tokio::spawn(async move {
        while let Some(conn) = stall_listener.accept().await {
            tokio::spawn(async move {
                // Drain inbound forever; never send anything back.
                while conn.recv().await.is_some() {}
            });
        }
    });

    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup = supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b_addr)]);
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));

    // Before the timeout: connected but stalled → no terminal Noop yet.
    tick(200).await;
    assert!(!a_sup.all_bootstrapped(), "stalled mid-scan, not yet complete");

    // Past the 2s hard timeout → best-effort complete.
    tick(2_500).await;
    assert!(
        a_sup.bootstrap_complete("B"),
        "stalled bootstrap completes best-effort on the hard timer"
    );
    assert!(a_sup.all_bootstrapped());
}

// ---------------------------------------------------------------------------
// lazy-batch lock discipline: a bootstrap scan of > one chunk interleaved with a
// concurrent put_call on B does not deadlock and A converges.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn lazy_batch_scan_interleaved_with_put_converges() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(51), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(52), 1, &net, &clock).await;

    let b_sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)]);
    b_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    // A forward-replicates 200 calls (> default chunk 128) into B's bak:A.
    let n = 200usize;
    for i in 0..n {
        let c = cref("A", &i.to_string());
        a.store
            .put_call(PRI, "A", &c, format!("b{i}").into_bytes(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }
    tick(300).await;

    // A reboots empty; bootstrap streams 200 keys across multiple batches.
    let a2_store = ReplicatingCallStore::new(2, clock.clone());
    let a2_sup = supervisor_for("A", &a2_store, &net, &clock, vec![("B".into(), b.addr)]);
    a2_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    // Interleave a concurrent reverse put on B mid-bootstrap (lock discipline).
    tick(50).await;
    let extra = cref("A", "0");
    b.store
        // Reverse/takeover bumps b (p stays at the branch point 1): (1,0) → (1,1).
        .put_call(BAK, "A", &extra, b"concurrent".to_vec(), &[], 0, 1, 1, &rev("A"))
        .await
        .unwrap();
    tick(600).await;

    // Converged: all 200 present as pri:A (call 0 carries the newer concurrent body).
    for i in 1..n {
        let c = cref("A", &i.to_string());
        assert_eq!(
            a2_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
            Some(&format!("b{i}").into_bytes()[..]),
            "call {i} converged on rebooted A"
        );
    }
    assert_eq!(
        a2_store.get_call(PRI, "A", &extra).await.unwrap().as_deref(),
        Some(&b"concurrent"[..]),
        "concurrent mutation wins by call_gen"
    );
    assert!(a2_sup.bootstrap_complete("B"));
}

// ---------------------------------------------------------------------------
// empty bak partition: A bootstraps from B that holds nothing for A → immediate
// terminal Noop(W) → A is bootstrap-complete and tails normally.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn empty_bak_partition_immediate_terminal_noop() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = Node::spawn("B", addr(62), 1, &net, &clock).await;

    // A cold-boots and bootstraps from B, which holds nothing in bak:A.
    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup = supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b.addr)]);
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    tick(150).await;

    assert!(
        a_sup.bootstrap_complete("B"),
        "empty bootstrap still hits the terminal Noop → complete"
    );
    assert!(a_sup.all_bootstrapped());

    // Tails normally afterward: B reverse-mutates one of A's calls → A picks up.
    let c = cref("A", "7");
    b.store
        .put_call(BAK, "A", &c, b"later".to_vec(), &[], 0, 1, 0, &rev("A"))
        .await
        .unwrap();
    tick(200).await;
    assert_eq!(
        a_store.get_call(PRI, "A", &c).await.unwrap().as_deref(),
        Some(&b"later"[..]),
        "A tails normally after an empty bootstrap"
    );
    // Watermark advanced past (0,0) once the tail Noop / delta landed.
    assert!(a_sup.watermark("B") >= Watermark::new(1, 0));
}

// ---------------------------------------------------------------------------
// Review regression (#9): an unreachable, NEVER-connected peer that only goes
// bootstrap-complete via the hard timer must NOT pin readiness NotReady — the
// node must boot and serve (Decision 4). A reachable-then-blipped peer keeps the
// strict sticky-current gate (covered elsewhere).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn readiness_not_pinned_by_unreachable_peer() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(61); // B never listens → every connect is refused.

    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup = supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b_addr)]);
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));

    // Before the hard timeout: not current (still trying to reach B).
    tick(200).await;
    assert!(!a_sup.all_current(), "not current before the hard timer fires");

    // Past the 2s hard timeout: bootstrap-complete best-effort, and readiness is
    // NOT pinned by the unreachable peer (it was never connected).
    tick(2_500).await;
    assert!(a_sup.all_bootstrapped(), "boots best-effort despite unreachable B");
    assert!(
        a_sup.all_current(),
        "an unreachable, never-connected peer must not pin NotReady"
    );
}

// ---------------------------------------------------------------------------
// Review regression (#5): a node whose bootstrap hard timer fired against an
// unreachable peer (bootstrap_complete=true, W still (0,0)) must STILL bootstrap
// — not cold-Replog — once the peer becomes reachable, or it silently misses the
// `bak:{me}` backups the peer holds (which live only in the peer's bak keyset,
// never in its changelog-for-me).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cold_start_bootstraps_after_hard_timeout_reconnect() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(72);
    let ca = cref("A", "1");

    // A is cold and pulls B; B is NOT listening yet → A's connect is refused and
    // the bootstrap hard timer trips (complete best-effort, W still (0,0)).
    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup = supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b_addr)]);
    a_sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B")],
        clock.clone(),
    )));
    tick(2_500).await;
    assert!(a_sup.bootstrap_complete("B"), "hard timer completed bootstrap best-effort");
    assert!(
        a_store.get_call(PRI, "A", &ca).await.unwrap().is_none(),
        "nothing reclaimed while B was unreachable"
    );

    // B comes up holding A's call as a bak:A backup (the apply-path shape: stored
    // with peer:None so it is NOT in B's changelog-for-A), and starts serving.
    let b_changelog = Changelog::new(1, clock.clone());
    let b_store = ReplicatingCallStore::with_changelog(b_changelog.clone(), clock.clone());
    b_store
        .put_call(BAK, "A", &ca, b"v1".to_vec(), &[], 0, 1, 0, &PutOpts::default())
        .await
        .unwrap();
    let b_listener = net.listen(b_addr).await.unwrap();
    tokio::spawn(ReplServer::new("B", b_changelog, Arc::new(b_store.clone())).run(b_listener));

    // A reconnects: because its watermark is still (0,0) it must BOOTSTRAP (scan
    // B's bak:A keyset) and reclaim `ca` as pri:A — not cold-Replog past it.
    tick(2_000).await;
    assert_eq!(
        a_store.get_call(PRI, "A", &ca).await.unwrap().as_deref(),
        Some(&b"v1"[..]),
        "after the peer becomes reachable the cold node bootstraps its bak backups"
    );
}
