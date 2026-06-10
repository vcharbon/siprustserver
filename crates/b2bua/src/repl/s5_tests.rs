//! S5 tests: the replication serve-loop + puller FSM + supervisor.
//!
//! All run under `#[tokio::test(start_paused = true)]`; the protocol is driven
//! BETWEEN `advance`s (advance to the deadline, let frames land, advance again)
//! per the CLAUDE.md fake-clock hazards. Transit delay is `>= 1 ms`.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::{Fault, ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Membership, Peer, SimulatedMembership};

use super::test_support::{cref, fwd, supervisor_for, tick, Node};
use super::{Changelog, FnPeerResolver, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor};
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

/// Short backoff so a couple of advances trip a reconnect deterministically.
fn fast_backoff() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        ..PullerConfig::default()
    }
}

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9000 + n))
}

// ---------------------------------------------------------------------------
// VERTICAL SKELETON — the gate.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn vertical_skeleton_put_on_a_appears_on_b() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    let a = Node::spawn("A", addr(1), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(2), 1, &net, &clock).await;

    // B pulls A.
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    let membership: Arc<dyn Membership> =
        Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone()));
    sup.start(membership);

    // Let the puller connect + open the subscription.
    tick(50).await;

    // A (primary) puts a call destined for B (backup).
    let call_ref = cref("A", "1");
    a.store
        .put_call(PRI, "A", &call_ref, b"body-A1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();

    // Advance: the Data frame propagates A → B.
    tick(200).await;

    // THE GATE: B's store has (Backup, "A", callRefA) with the same body.
    let got = b.store.get_call(BAK, "A", &call_ref).await.unwrap();
    assert_eq!(
        got.as_deref(),
        Some(&b"body-A1"[..]),
        "B must hold the backup body A pushed"
    );
}

// ---------------------------------------------------------------------------
// convergence / steady-state: puts + update + delete.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn convergence_update_and_delete() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(11), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(12), 1, &net, &clock).await;
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    let c1 = cref("A", "1");
    let c2 = cref("A", "2");
    a.store
        .put_call(PRI, "A", &c1, b"v1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    a.store
        .put_call(PRI, "A", &c2, b"w1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    tick(50).await;
    assert!(sup.is_current("A"), "current after first Noop");

    // Update c1 (higher call_gen) + delete c2.
    a.store
        .put_call(PRI, "A", &c1, b"v2".to_vec(), &[], 0, 2, 0, &fwd("B"))
        .await
        .unwrap();
    a.store
        .delete_call(PRI, "A", &c2, &[], &fwd("B"))
        .await
        .unwrap();
    tick(50).await;

    assert_eq!(
        b.store.get_call(BAK, "A", &c1).await.unwrap().as_deref(),
        Some(&b"v2"[..]),
        "update shows latest body on B"
    );
    assert!(
        b.store.get_call(BAK, "A", &c2).await.unwrap().is_none(),
        "delete removes c2 on B"
    );
}

// ---------------------------------------------------------------------------
// current-on-Noop sticky across a cut+reconnect.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn current_flag_sticky_across_reconnect() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(21), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(22), 1, &net, &clock).await;
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    // Bootstrap pre-seed → cold-Replog re-pull → first tail Noop sets `current`;
    // the two-phase re-hydration (S6) needs a slightly larger advance budget than
    // a bare Replog open.
    tick(300).await;
    assert!(sup.is_current("A"), "current after first Noop");

    // Cut the A→B direction (server pushes won't arrive; recv yields None).
    let local_pairs = (b.addr, a.addr);
    net.apply_fault(Fault::Partition {
        a: local_pairs.0,
        b: local_pairs.1,
    });
    // Drive past the cut detection + a backoff so the puller cycles.
    tick(50).await;
    assert!(sup.is_current("A"), "current stays sticky after a cut");

    net.apply_fault(Fault::Heal {
        a: local_pairs.0,
        b: local_pairs.1,
    });
    tick(300).await;
    assert!(sup.is_current("A"), "still current after reconnect");
}

// ---------------------------------------------------------------------------
// watermark retention: cut after 3, add 2 more, reconnect pulls only deltas.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn watermark_retention_pulls_only_deltas() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(31), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(32), 1, &net, &clock).await;
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    for i in 0..3 {
        let c = cref("A", &i.to_string());
        a.store
            .put_call(PRI, "A", &c, format!("b{i}").into_bytes(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }
    tick(50).await;
    // The data B holds for A rides the **Backup** flow (B backs up A's Forward-
    // flushed calls); its watermark is the one that tracks these deltas (the
    // Reclaim flow's cursor is over A's empty `bak:{B}` keyspace).
    let w_after_3 = sup.flow_watermark("A", Partition::Bak);
    assert_eq!(w_after_3, Watermark::new(1, 3), "B tailed 3 entries");

    // Cut, then add 2 more while disconnected.
    net.apply_fault(Fault::Partition { a: b.addr, b: a.addr });
    tick(50).await;
    for i in 3..5 {
        let c = cref("A", &i.to_string());
        a.store
            .put_call(PRI, "A", &c, format!("b{i}").into_bytes(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }

    // Reconnect from retained W=(1,3): only the 2 new deltas should flow.
    net.apply_fault(Fault::Heal { a: b.addr, b: a.addr });
    tick(400).await;

    // Converged: all 5 present, watermark advanced to head.
    for i in 0..5 {
        let c = cref("A", &i.to_string());
        assert_eq!(
            b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
            Some(&format!("b{i}").into_bytes()[..]),
            "call {i} converged on B"
        );
    }
    assert_eq!(sup.flow_watermark("A", Partition::Bak), Watermark::new(1, 5), "W at head after deltas");
}

// ---------------------------------------------------------------------------
// backoff + reconnect: cut, assert backoff grows, heal, converge.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn backoff_then_reconnect_converges() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    // A is absent at first (no listener) so connect fails → backoff grows.
    let b_addr = addr(42);
    let a_addr = addr(41);

    let b_changelog = Changelog::new(1, clock.clone());
    let b_store = ReplicatingCallStore::with_changelog(b_changelog, clock.clone());
    let b_listener = net.listen(b_addr).await.unwrap();
    tokio::spawn(ReplServer::new("B", Changelog::new(1, clock.clone()), Arc::new(b_store.clone())).run(b_listener));

    let sup = supervisor_for("B", &b_store, &net, &clock, vec![("A".into(), a_addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));

    // First connect fails immediately (no listener at A). Backoff = 100ms.
    tick(50).await;
    assert!(!sup.is_current("A"), "no peer yet");
    // Drive through two backoff cycles (100, 200) of failed connects.
    tick(400).await;

    // Now bring A up (heal by spawning its listener + server) and a call.
    let a = Node::spawn("A", a_addr, 1, &net, &clock).await;
    let c = cref("A", "1");
    a.store
        .put_call(PRI, "A", &c, b"late".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();

    // Advance past the (now larger) backoff so the puller retries + converges.
    tick(2_000).await;
    assert_eq!(
        b_store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"late"[..]),
        "B converges once A comes up after backoff"
    );
}

// ---------------------------------------------------------------------------
// topology reconcile: add A, converge; remove → park (W retained); re-add.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn topology_add_remove_readd() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(51), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(52), 1, &net, &clock).await;

    // Start with A absent in membership; B has no puller.
    let membership = SimulatedMembership::with_clock(vec![], clock.clone());
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(membership.clone()));
    tick(50).await;
    assert!(!sup.is_running("A"), "no puller before A is added");

    // Add A → B spawns a puller and converges.
    membership.add(Peer::new("A", "A"));
    tick(50).await;
    let c1 = cref("A", "1");
    a.store
        .put_call(PRI, "A", &c1, b"x1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    tick(50).await;
    assert!(sup.is_running("A"));
    assert_eq!(
        b.store.get_call(BAK, "A", &c1).await.unwrap().as_deref(),
        Some(&b"x1"[..])
    );
    // The backup data B holds for A rides the Backup flow; its watermark tracks
    // these deltas (Reclaim is over A's empty `bak:{B}`).
    let w_before_remove = sup.flow_watermark("A", Partition::Bak);
    assert_eq!(w_before_remove, Watermark::new(1, 1));

    // Remove A → puller parks; W retained.
    membership.remove("A");
    tick(50).await;
    assert!(!sup.is_running("A"), "puller parked on Removed");
    assert_eq!(sup.flow_watermark("A", Partition::Bak), w_before_remove, "W retained across Park");

    // While parked, A adds another call.
    let c2 = cref("A", "2");
    a.store
        .put_call(PRI, "A", &c2, b"x2".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();

    // Re-add A → reconnect from retained W=(1,1), pull only the new delta.
    membership.add(Peer::new("A", "A"));
    tick(100).await;
    assert!(sup.is_running("A"));
    assert_eq!(
        b.store.get_call(BAK, "A", &c2).await.unwrap().as_deref(),
        Some(&b"x2"[..]),
        "B re-acquires the new delta after re-add"
    );
}

// ---------------------------------------------------------------------------
// LWW idempotence: replay applies once; a lower call_gen never overwrites.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn lww_idempotence_and_no_regression() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(61), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(62), 1, &net, &clock).await;
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    let c = cref("A", "1");
    // gen 5 body.
    a.store
        .put_call(PRI, "A", &c, b"high".to_vec(), &[], 0, 5, 0, &fwd("B"))
        .await
        .unwrap();
    tick(50).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"high"[..])
    );

    // Cut + reconnect from (0,0) replays the SAME entry (idempotent). The body
    // is unchanged; no regression. (call_gen equal → body write skipped, W still
    // advances.)
    net.apply_fault(Fault::Partition { a: b.addr, b: a.addr });
    tick(50).await;
    net.apply_fault(Fault::Heal { a: b.addr, b: a.addr });
    tick(300).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"high"[..]),
        "replay does not corrupt the stored body"
    );

    // A lower call_gen (3) directly applied on B must NOT overwrite the high (5)
    // body. We exercise the puller's LWW by writing a stale frame through B's
    // store directly with a lower gen is not the path; instead push from A with
    // a LOWER gen — the changelog bumps but B's LWW skips the body write.
    a.store
        .put_call(PRI, "A", &c, b"stale".to_vec(), &[], 0, 3, 0, &fwd("B"))
        .await
        .unwrap();
    tick(100).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
        Some(&b"high"[..]),
        "lower call_gen must not overwrite the higher one on B"
    );
}

// ---------------------------------------------------------------------------
// cold reboot convergence: clear B's store, reconnect from (0,0).
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn cold_reboot_reacquires_full_set() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(71), 1, &net, &clock).await;
    let b = Node::spawn("B", addr(72), 1, &net, &clock).await;
    let sup = supervisor_for("B", &b.store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(50).await;

    for i in 0..3 {
        let c = cref("A", &i.to_string());
        a.store
            .put_call(PRI, "A", &c, format!("b{i}").into_bytes(), &[], 0, 1, 0, &fwd("B"))
            .await
            .unwrap();
    }
    tick(50).await;

    // Simulate B reboot: brand-new empty store + supervisor pulling from (0,0).
    let b2_store = ReplicatingCallStore::new(1, clock.clone());
    let sup2 = supervisor_for("B", &b2_store, &net, &clock, vec![("A".into(), a.addr)], fast_backoff());
    sup2.start(Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("A", "A")],
        clock.clone(),
    )));
    tick(500).await;

    // The compacted changelog delivers the full live set to the cold puller.
    for i in 0..3 {
        let c = cref("A", &i.to_string());
        assert_eq!(
            b2_store.get_call(BAK, "A", &c).await.unwrap().as_deref(),
            Some(&format!("b{i}").into_bytes()[..]),
            "cold puller re-acquires call {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// REGRESSION (ADR-0012 D1): a LAGGED membership channel must still redirect the
// puller to a peer's NEW address. Before the fix the supervisor reconcile loop
// did `Err(_) => return` on `Lagged` and the node went permanently deaf — the
// puller stayed pinned to the dead pod IP (the endurance-run incident). Here the
// address-change delta is deliberately DROPPED from the broadcast ring (buried
// under >256 throwaway deltas), so the only way the puller can redirect is the
// `Lagged → snapshot reconcile` path.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn lagged_membership_channel_still_redirects_puller_to_new_addr() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    let a1_addr = addr(81);
    let a2_addr = addr(82);
    let b_addr = addr(83);

    // A's first life at a1; B pulls it and converges on c1.
    let a1 = Node::spawn("A", a1_addr, 1, &net, &clock).await;
    let b = Node::spawn("B", b_addr, 1, &net, &clock).await;

    // A resolver that parses the peer's `host` as a SocketAddr, so a membership
    // address-change actually MOVES where the puller connects (the fixed
    // ordinal→addr map of `supervisor_for` cannot express a move; the real
    // host→addr resolver can, and that is what D3 exercises).
    let resolve = Arc::new(FnPeerResolver(|peer: &Peer| {
        peer.host.parse::<SocketAddr>().unwrap()
    }));
    let membership =
        SimulatedMembership::with_clock(vec![Peer::new("A", a1_addr.to_string())], clock.clone());
    let sup = ReplicationSupervisor::with_config(
        "B",
        net.clone(),
        b.store.clone(),
        resolve,
        clock.clone(),
        fast_backoff(),
    );
    sup.start(Arc::new(membership.clone()));

    let c1 = cref("A", "1");
    a1.store
        .put_call(PRI, "A", &c1, b"one".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    tick(300).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c1).await.unwrap().as_deref(),
        Some(&b"one"[..]),
        "B converged on A@a1 before the restart"
    );

    // A "restarts" at a2 (a higher incarnation gen) holding a fresh call c2.
    let a2 = Node::spawn("A", a2_addr, 2, &net, &clock).await;
    let c2 = cref("A", "2");
    a2.store
        .put_call(PRI, "A", &c2, b"two".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();

    // Emit the CRITICAL delta (A: a1 → a2) FIRST, then bury it under >256 throwaway
    // deltas — all synchronously, so the supervisor task stays parked and the
    // broadcast ring (capacity 256) DROPS the address-change. The supervisor can
    // now only learn the new address by reconciling from the snapshot after it
    // observes `Lagged`.
    membership.change_address(Peer::new("A", a2_addr.to_string()));
    for _ in 0..150 {
        membership.add(Peer::new("Z", "127.0.0.1:1"));
        membership.remove("Z");
    }

    // Wake the supervisor: recv() → Lagged → reconcile_from_snapshot → snapshot
    // shows A@a2 (host drifted) → puller respawned to a2 → converges on c2.
    tick(600).await;
    assert_eq!(
        b.store.get_call(BAK, "A", &c2).await.unwrap().as_deref(),
        Some(&b"two"[..]),
        "after a Lagged delta the puller redirected to A's new address and pulled c2"
    );
}

/// The catch-up `Noop` carries the FLOW's own cursor — the last `Data.at`
/// actually sent — never a separately-read global `head()`. The head counter is
/// shared across every (peer, partition) sub-log, so jumping the cursor to it
/// persisted OTHER flows' counters into this puller's watermark; a bump for THIS
/// flow landing between the drain snapshot and the head read was then excluded
/// from every future drain (`Bound::Excluded`) and survived reconnects — a
/// missed DELETE never re-bumps, so the backup held the dead call until its
/// body-TTL backstop.
#[tokio::test(start_paused = true)]
async fn catchup_noop_carries_flow_cursor_not_global_head() {
    use std::time::Duration;

    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let a = Node::spawn("A", addr(61), 1, &net, &clock).await;

    // One entry for peer B (global counter 1) and one for peer C (counter 2):
    // the global head is (1,2); B's Bak sub-log tops out at (1,1).
    let c1 = cref("A", "1");
    let c2 = cref("A", "2");
    a.store
        .put_call(PRI, "A", &c1, b"v1".to_vec(), &[], 0, 1, 0, &fwd("B"))
        .await
        .unwrap();
    a.store
        .put_call(PRI, "A", &c2, b"v1".to_vec(), &[], 0, 1, 0, &fwd("C"))
        .await
        .unwrap();

    // Hand-rolled WARM Backup-flow pull as B (warm ⇒ no bootstrap scan).
    let conn = net.connect(a.addr).await.unwrap();
    conn.send(Frame::PullRequest {
        proto_ver: 3,
        caller: "B".into(),
        partition: Partition::Bak,
        since: Watermark::new(1, 0),
    })
    .await
    .unwrap();

    let mut last_data_at = None;
    let mut noop_at = None;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(1_000), conn.recv()).await {
            Ok(Some(Frame::Data { at, .. })) => last_data_at = Some(at),
            Ok(Some(Frame::Noop { at })) => {
                noop_at = Some(at);
                break;
            }
            _ => break,
        }
    }

    assert_eq!(last_data_at, Some(Watermark::new(1, 1)), "B's one Data frame");
    assert_eq!(
        noop_at,
        Some(Watermark::new(1, 1)),
        "the catch-up Noop must carry B's flow cursor (1,1), not the global head (1,2)"
    );
}
