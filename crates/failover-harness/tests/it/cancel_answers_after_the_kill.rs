//! **A CANCEL whose answers die with the node is answered by the backup.**
//! The callee rings; the caller CANCELs. The primary's transaction layer
//! holds the 200 to the CANCEL and the 487 to the INVITE until the turn that
//! took the CANCEL releases them after its write (ADR-0055 §3), and the
//! primary dies with that turn's write logged, never applied. The caller,
//! with no answer to her CANCEL, repeats it on Timer E (RFC 3261 §17.1.2.2);
//! the repeat reaches the backup, whose takeover copy answers it 200 and 487
//! on the INVITE transaction it seeded and CANCELs the callee: nothing but
//! the 487 ends the caller's INVITE, and the callee rings no more.

use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::cluster::{Cluster, ClusterRecipe};
use failover_harness::sweep::{ALICE, BOB};
use failover_harness::{total_cdrs_for, FailoverHarness, Incident, TurnAt, TurnPoint};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::call_terminate_on_backup::OFFER;
use crate::departure_support::{bring_up_deciding, kill_announced, placed, CountedDecision};
use crate::inflight_resend::{last_request_sent, split_mut, survivor_of};

#[tokio::test(start_paused = true)]
async fn a_cancel_lost_with_its_node_is_answered_by_the_backup_and_cancels_the_callee() {
    let name = "cancel-answers-after-the-kill";
    let recipe = ClusterRecipe::routing_to(BOB.parse().unwrap());
    let mut cluster = Cluster::start(name, &recipe).await;
    let alice = cluster.fh.agent("alice", ALICE).await;
    let bob = cluster.fh.agent("bob", BOB).await;
    let placement = cluster.place_next_call(&alice, "b1");
    let reached = cluster.fh.turn_points().arm(TurnAt { turn: 3, point: TurnPoint::Logged });

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(cluster.proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    cluster.fh.advance(Duration::from_millis(100)).await;
    let mut cancel = call.cancel().await;
    let held = reached.await.expect("the CANCEL's turn logs its write");
    assert_eq!(held.node, placement.primary, "the turn is held at the primary");
    cluster.kill(&placement.primary, &placement.backup);
    held.release();

    // The caller hears nothing and repeats her CANCEL on Timer E.
    cluster.fh.advance(Duration::from_millis(500)).await;
    let (wire, dst) = last_request_sent(&cluster.fh, alice.addr(), &Method::Cancel);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's repeat leaves");
    cancel.expect(200).await;
    call.expect(487).await;
    let mut callee_cancel = bob.receive("CANCEL").await;
    callee_cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    cluster.fh.advance(Duration::from_secs(40)).await;
    let parser = CustomParser::new();
    let to_alice: Vec<(u16, Method)> = cluster
        .fh
        .sip_entries()
        .into_iter()
        .filter(|e| e.to == alice.addr())
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) => Some((r.status(), r.cseq().method().clone())),
            _ => None,
        })
        .collect();
    let at_487 = to_alice.iter().position(|(s, _)| *s == 487).expect("a 487");
    let after: Vec<_> =
        to_alice[at_487..].iter().filter(|(s, m)| *m == Method::Invite && *s != 487).collect();
    assert!(after.is_empty(), "nothing but the 487 ends the caller's INVITE: {after:?}");
    let call_ref = cluster.fh.turn_points().first_call().expect("a call").0;
    cluster.node(&placement.backup).simulate_peer_removed(&placement.primary);
    let drained = cluster.fh.settle_terminal(async || cluster.is_clear_of(&call_ref).await).await;
    assert!(drained, "the cluster forgets the call");
    cluster.fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    cluster.fh.assert_sip_rfc_clean(name);
}

/// The same loss before the decision: the caller CANCELs a call still asking
/// its decision, and its node dies holding the answers. The backup asks the
/// decision again (ADR-0055 §11); the caller's repeated CANCEL reaches it, is
/// answered 200 and 487, and the decision that returns dials nobody.
#[tokio::test(start_paused = true)]
async fn an_undecided_cancel_lost_with_its_node_is_answered_by_the_backup_which_dials_nobody() {
    let name = "undecided-cancel-answers-after-the-kill";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, Duration::from_secs(3));
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let mut cancel = call.cancel().await;
    fh.advance(Duration::from_millis(20)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.strike(Incident::PrimaryKilled);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Cancel);
    alice.try_send_datagram(&wire, dst).await.expect("the caller's repeat leaves");
    cancel.expect(200).await;
    call.expect(487).await;
    fh.advance(Duration::from_secs(10)).await;
    assert!(!fh.sip_entries().iter().any(|e| e.to == bob.addr()), "the callee is never dialled");

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let released = fh
        .settle_terminal(async || {
            survivor.memory_clean() && !survivor.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(released, "nothing of the call is left live or stored");
    assert_eq!(total_cdrs_for(&[survivor], &call_ref), 1, "the backup writes the one CDR");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
