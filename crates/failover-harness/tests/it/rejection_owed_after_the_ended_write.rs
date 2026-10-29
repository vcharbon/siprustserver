//! **The caller's rejection outlives the node that decided it.** The callee
//! refuses the call 486, and the primary's turn that relays it ends the call.
//! The turn writes the ended call to the backup before the final leaves, and
//! deletes it only once the final left (ADR-0055 §3). The primary dies with
//! the backup holding that write and the final still unsent, the caller in
//! `Proceeding` after the 100 she heard (RFC 3261 §17.1.1.2). The backup
//! that discharges the ended call at the departure sends her the 486 on a
//! Completed transaction that absorbs her ACK (§17.2.1). The ACK of the
//! callee's 486 died held with the node too (ADR-0055 §3): the backup ACKs
//! the 486 the callee repeats for want of it (§17.1.1.2).

use std::time::Duration;

use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{ALICE, BOB};
use failover_harness::{TurnAt, TurnPoint};

use crate::call_terminate_on_backup::OFFER;

/// The callee refuses the call placed on `b1`; `b1` dies, announced, where
/// the backup applied the ending turn's write.
#[tokio::test(start_paused = true)]
async fn a_rejection_lost_with_its_node_after_the_ended_write_applied_reaches_the_caller() {
    let name = "rejection-owed-after-the-ended-write";
    let recipe = ClusterRecipe::routing_to(BOB.parse().unwrap());
    let mut cluster = Cluster::start(name, &recipe).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    let placement = cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: 2, point: TurnPoint::Applied });

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    call.expect(100).await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    let held = reached.await.expect("the backup applies the ending turn's write");
    assert_eq!(held.node, placement.backup);
    let call_ref = held.call_ref.clone();
    cluster.kill(&placement.primary, &placement.backup);
    held.release();

    call.expect(486).await;
    // The callee repeats its 486 on Timer G (RFC 3261 §17.2.1).
    cluster.fh.advance(Duration::from_millis(500)).await;
    assert!(bob.repeat_unacked_finals().await, "the callee's 486 awaits its ACK");
    bob.receive("ACK").await;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    cluster.fh.assert_sip_rfc_clean(name);
}
