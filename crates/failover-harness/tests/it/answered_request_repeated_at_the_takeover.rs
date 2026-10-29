//! **A copy repeats the final its record holds for a request it never
//! answered on the wire.** The caller's in-dialog INFO is relayed to the
//! callee, whose 200 the primary relays back; the backup applies that turn's
//! write and the primary dies before the 200 leaves. The caller's INFO goes
//! again on Timer E (RFC 3261 §17.1.2.2), sent here by hand, and reaches the backup, whose record
//! holds the request as answered: its copy repeats the 200 (§17.2.2), and the
//! callee is sent the INFO once.
//!
//! ```text
//!   alice ─INFO─▶ b1 ─INFO─▶ bob ─200─▶ b1; the write applied at b2
//!                 ✗ b1 killed before its 200 leaves ✗
//!   alice ─INFO (Timer E)─▶ b2 ─200─▶ alice; bob sees one INFO
//! ```

use std::time::Duration;

use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{ALICE, BOB};
use failover_harness::{assert_call_fully_over, TurnAt, TurnPoint};
use sip_message::generators::InDialogMethod;
use sip_message::Method;

use crate::call_terminate_on_backup::{ANSWER, OFFER};
use crate::inflight_resend::last_request_sent;

/// The INFO requests that reached `to`.
fn infos_to(cluster: &Cluster, to: std::net::SocketAddr) -> usize {
    cluster.fh.sip_entries().iter().filter(|e| e.to == to && e.opens_with("INFO ")).count()
}

#[tokio::test(start_paused = true)]
async fn the_backup_repeats_the_relayed_200_the_dead_primary_never_sent() {
    let name = "answered-request-repeated-at-the-takeover";
    let recipe = ClusterRecipe::routing_to(BOB.parse().unwrap());
    let mut cluster = Cluster::start(name, &recipe).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    let placement = cluster.place_next_call(&alice, "b1");

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    cluster.fh.advance(Duration::from_millis(500)).await;
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;

    // The INFO's turn relays it; the next, taken on the callee's 200, relays
    // the 200 to the caller and is held once the backup applied its write.
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    let relayed = cluster.fh.turn_points().turns(&call_ref).len() + 2;
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: relayed, point: TurnPoint::Applied });
    bob.receive("INFO").await.respond(200, "OK").await;
    let held = reached.await.expect("the backup applies the 200's turn");
    assert_eq!(held.node, placement.backup, "held where the backup applied the write");
    cluster.kill(&placement.primary, &placement.backup);
    held.release();

    // Timer E: the caller's INFO goes again, T1 after it first left.
    cluster.fh.advance(Duration::from_millis(500)).await;
    let (wire, dst) = last_request_sent(&cluster.fh, alice.addr(), &Method::Info);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's INFO goes again");
    info.expect(200).await;
    cluster.fh.advance(Duration::from_secs(2)).await;
    assert_eq!(infos_to(&cluster, bob.addr()), 1, "the callee is sent the INFO once");

    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS", "INFO"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&cluster.nodes(), &call_ref, &cluster.limiter).await;
    cluster.fh.assert_sip_rfc_clean(name);
}
