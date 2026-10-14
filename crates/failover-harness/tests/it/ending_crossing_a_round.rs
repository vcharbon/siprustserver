//! **A call's end crossing a re-INVITE round only one node holds.** The
//! proxy misroutes a request to the backup while the primary lives
//! (ADR-0014), and the replication between the two nodes is held one way, so
//! one node ends the call while the other holds a round it alone relayed.
//! The call ends once, and every round's originator draws a final for its
//! re-INVITE (RFC 3261 §15.1.2: a UAS ending a dialog still answers its
//! pending requests):
//!
//! - the copy ends the call while the primary holds the caller's round: the
//!   primary takes the end, answers the caller's re-INVITE and discharges the
//!   call — one CDR, nothing left;
//! - the primary ends the call while the copy holds the callee's round: the
//!   copy keeps the round until its final reaches the callee, through the
//!   primary's writes and through its delete of the call.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, OFFER};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// Nothing of the call is left anywhere, one CDR.
async fn settle(est: &Established) {
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

/// The primary's writes are held from the copy while it relays the caller's
/// re-INVITE; the callee holds its answer. The proxy reads the primary dead:
/// the callee hangs up through the copy, which ends the call; the callee then
/// refuses the round 487. Once the primary's writes are released, the copy's
/// ending is the call's.
#[tokio::test(start_paused = true)]
async fn the_copys_ending_ends_the_call_the_primary_holds_a_round_in() {
    let mut est = establish("ending-crossing-the-primarys-round").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&p_ord, &b_ord);
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut bye = est.bob_dialog.bye().await;
    est.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    est.fh.advance(SETTLE).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.advance(Duration::from_millis(100)).await;
    at_bob.respond(487, "Request Terminated").await;
    round.expect(487).await;
    est.fh.resume(&p_ord, &b_ord);
    est.fh.advance(SETTLE).await;
    settle(&est).await;
}

/// The proxy reads the primary dead: the callee's re-INVITE reaches the copy,
/// which relays it to the caller, who holds her answer; the copy's writes are
/// then held from the primary. The proxy reads the primary alive: the caller
/// hangs up through the primary, the callee holding its answer to the BYE.
/// Released, the copy's write is refused at the primary, which is ending the
/// call. The caller refuses the round 487, which reaches the callee through
/// the copy; then the callee answers the BYE.
#[tokio::test(start_paused = true)]
async fn the_copy_keeps_its_round_through_the_primarys_ending() {
    let mut est = establish("primary-ending-crossing-the-copys-round").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    est.fh.stall(&b_ord, &p_ord);
    let mut round = est.bob_dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_alice = est.alice.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    let mut bye = est.dialog.bye().await;
    let mut bye_at_bob = est.bob.receive("BYE").await;
    est.fh.resume(&b_ord, &p_ord);
    est.fh.advance(SETTLE).await;

    at_alice.respond(487, "Request Terminated").await;
    round.expect(487).await;
    bye_at_bob.respond(200, "OK").await;
    bye.expect(200).await;
    settle(&est).await;
}

/// The same, but the callee answers the BYE first: the primary ends the call
/// and deletes it while the copy still holds the callee's round. The copy
/// keeps it through the delete, and the caller's 487 still reaches the
/// callee.
#[tokio::test(start_paused = true)]
async fn the_copy_keeps_its_round_through_the_primarys_delete() {
    let mut est = establish("primary-delete-crossing-the-copys-round").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    est.fh.stall(&b_ord, &p_ord);
    let mut round = est.bob_dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_alice = est.alice.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    let mut bye = est.dialog.bye().await;
    est.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    est.fh.resume(&b_ord, &p_ord);
    est.fh.advance(SETTLE).await;

    at_alice.respond(487, "Request Terminated").await;
    round.expect(487).await;
    settle(&est).await;
}
