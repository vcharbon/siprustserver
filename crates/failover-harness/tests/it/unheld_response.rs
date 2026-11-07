//! **A response for a call no node holds** is answered as an unknown dialog
//! would be (ADR-0055 §4): a non-2xx final to an INVITE is ACKed hop by hop
//! at the Request-URI the sender's via-extension states (RFC 3261 §17.1.1.3,
//! ADR-0055 §12), so the callee stops retransmitting it (§17.2.1).
//!
//! Beyond the failover sweep: a 487 failed over to the node that released the call is ACKed.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_over, assert_call_fully_released, total_cdrs_for, worker_ordinals,
    FailoverHarness, WorkerHealth,
};
use scenario_harness::callflow::ANSWER_SDP;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, OFFER};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The primary relays the caller's re-INVITE while its writes are held from
/// the backup; the callee holds its final. The proxy reads the primary dead:
/// the callee hangs up through the copy, which ends the call, and the primary
/// takes that ending: it answers the caller's re-INVITE 487 and CANCELs the
/// round toward the callee. The proxy still reads the primary dead, so the
/// callee's 487 reaches the backup, which no longer holds the call: it ACKs
/// the 487 from the via-extension, and the callee's INVITE transaction
/// completes.
#[tokio::test(start_paused = true)]
async fn a_487_failed_over_to_the_node_that_released_the_call_is_acked() {
    let mut est = establish("unheld-487-acked").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&p_ord, &b_ord);
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut bye = est.bob_dialog.bye().await;
    est.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    round.expect(487).await;
    est.bob.receive("CANCEL").await.respond(200, "OK").await;
    est.fh.advance(SETTLE).await;
    at_bob.respond(487, "Request Terminated").await;
    let ack = est.bob.receive("ACK").await;
    assert_eq!(
        ack.request().top_via().branch(),
        at_bob.request().top_via().branch(),
        "the ACK of a non-2xx rides the INVITE's own transaction (RFC 3261 §17.1.1.3)"
    );
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.resume(&p_ord, &b_ord);
    est.fh.advance(SETTLE).await;
    let (store, w_b1, w_b2, call_ref) = (&est.store, &est.w_b1, &est.w_b2, &est.call_ref);
    let drained = est
        .fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(call_ref).await
                && !w_b2.holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    est.fh.linger_peers(&[&est.alice, &est.bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[w_b1, w_b2], call_ref, store).await;
}

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const LB: &str = "127.0.0.1:5080";
const HOP: &str = "127.0.0.1:5075";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

fn addr(at: &str) -> SocketAddr {
    at.parse().expect("an address")
}

/// The sequence above on a dialog whose route set holds a hop past the proxy: the
/// re-INVITE reaches that hop, whose server transaction the ACK must reach (RFC
/// 3261 §17.1.1.2) though the node ACKing holds no record of the request. Its
/// via-extension kept the route (ADR-0055 §12).
#[tokio::test(start_paused = true)]
async fn a_487_failed_over_is_acked_at_the_hop_its_invite_took() {
    let mut fh = FailoverHarness::new("unheld-487-hop", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let hop = fh.scripted_proxy("hop", HOP).await;
    let proxy = fh.spawn_proxy(LB, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    let hop_at = ("127.0.0.1", 5075);
    let lb_at = ("127.0.0.1", 5080);
    let w_b1 = fh.spawn_worker("b1", "b1", B1, &["b2"], hop_at, lb_at).await;
    let w_b2 = fh.spawn_worker("b2", "b2", B2, &["b1"], hop_at, lb_at).await;
    fh.advance(SETTLE).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    hop.forward_request(addr(BOB)).await;
    let mut uas = bob.receive("INVITE").await;
    let (p_ord, b_ord) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    hop.forward_response(addr(LB)).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    hop.forward_request(addr(BOB)).await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();
    fh.advance(SETTLE).await;
    let backup = if p_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref = backup.scan_one_backed_up(&p_ord).await.expect("the replicated call");

    fh.stall(&p_ord, &b_ord);
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let forwarded = hop.forward_request(addr(BOB)).await;
    let branch = forwarded.top_via().branch().expect("the hop's branch").to_string();
    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(100, "Trying").await;
    hop.forward_response(addr(LB)).await;
    round.expect(100).await;
    fh.advance(Duration::from_millis(300)).await;
    proxy.set_health(&p_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    let mut bye = bob_dialog.bye().await;
    hop.forward_request(addr(LB)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    hop.forward_response(addr(BOB)).await;
    bye.expect(200).await;
    round.expect(487).await;
    hop.forward_request_on_branch(addr(BOB), &branch).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    hop.forward_response(addr(LB)).await;
    fh.advance(SETTLE).await;

    at_bob.respond(487, "Request Terminated").await;
    hop.forward_response(addr(LB)).await;
    let ack = hop.forward_request_on_branch(addr(BOB), &branch).await;
    assert_eq!(ack.method(), "ACK", "the hop takes the ACK of its own forward");
    assert_eq!(ack.cseq().seq(), at_bob.request().cseq().seq());
    bob.receive("ACK").await;

    proxy.set_health(&p_ord, WorkerHealth::Alive);
    fh.resume(&p_ord, &b_ord);
    fh.advance(SETTLE).await;
    let drained = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "the call has its one CDR");
}
