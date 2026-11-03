//! Readiness waits for the Backup flow of every ready peer (ADR-0014 §11),
//! bounded by the bootstrap hard timer (ADR-0011 X5).
//!
//! ```text
//!   held     B serves A's Reclaim flow, then stops listening: A's Backup
//!            flow is never reached ⇒ not current until the Backup flow's
//!            own hard timer, current after it
//!   carried  B holds a call A backs up ⇒ A reads current only once its
//!            Backup flow holds it
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, supervisor_for, tick, Node};
use super::ReplicatingCallStore;
use crate::store::{CallStore, PartitionRole};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9700 + n))
}

/// A peer at `at` that serves exactly one stream, the Reclaim flow, with an
/// empty partition (one catch-up `Noop`), and refuses every later connect.
async fn serve_one_reclaim_flow(net: &Arc<SimulatedReplicationNetwork>, at: SocketAddr) {
    let listener = net.listen(at).await.unwrap();
    tokio::spawn(async move {
        let conn = listener.accept().await.unwrap();
        drop(listener);
        let pulled = conn.recv().await;
        assert!(
            matches!(pulled, Some(Frame::PullRequest { partition: Partition::Pri, .. })),
            "the Reclaim flow opens first: {pulled:?}"
        );
        conn.send(Frame::Noop { at: Watermark::new(1, 0) }).await.unwrap();
        while conn.recv().await.is_some() {}
    });
}

#[tokio::test(start_paused = true)]
async fn a_backup_flow_never_reached_holds_readiness_until_its_hard_timer() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b_addr = addr(1);
    serve_one_reclaim_flow(&net, b_addr).await;

    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup =
        supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b_addr)], fast_config());
    a_sup
        .start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone())));

    // `tick(n)` advances `ceil(n / 100) + 1` chunks of 100 ms.
    tick(200).await;
    assert!(a_sup.is_current("B"), "the Reclaim flow reached B and caught up");
    assert!(a_sup.all_bootstrapped(), "A's own partition is re-hydrated");
    assert!(!a_sup.all_current(), "the Backup flow, never reached, still holds readiness");

    // 1.9 s in: the Backup flow's own 2 s hard timer, armed as it opened just
    // after the Reclaim flow caught up, has not run out.
    tick(1_500).await;
    assert!(!a_sup.all_current(), "a peer the Reclaim flow reached does not release it");
    tick(500).await;
    assert!(a_sup.all_current(), "its own hard timer releases the Backup flow never reached");
}

#[tokio::test(start_paused = true)]
async fn readiness_reads_current_only_once_the_backup_flow_holds_the_peers_calls() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = Node::spawn("B", addr(2), 1, &net, &clock).await;
    let c = cref("B", "1");
    b.store
        .put_call(PartitionRole::Primary, "B", &c, b"v1".to_vec(), &[], 0, 1, 0, &fwd("A"))
        .await
        .unwrap();

    let a_store = ReplicatingCallStore::new(1, clock.clone());
    let a_sup =
        supervisor_for("A", &a_store, &net, &clock, vec![("B".into(), b.addr)], fast_config());
    a_sup
        .start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone())));

    let mut held_when_current = None;
    for _ in 0..200 {
        sip_clock::testkit::advance_settled(std::time::Duration::from_millis(1)).await;
        if a_sup.all_current() {
            held_when_current =
                Some(a_store.get_call(PartitionRole::Backup, "B", &c).await.unwrap().is_some());
            break;
        }
    }
    assert_eq!(held_when_current, Some(true), "A reads current holding B's call it backs up");
}
