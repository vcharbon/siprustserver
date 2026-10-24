//! **A re-INVITE round the backup's copy opens, which its primary has not
//! read when it writes the call again.** The proxy misroutes the caller's
//! re-INVITE to the backup while the primary lives (ADR-0014); the copy
//! relays it, but its write does not reach the primary before the primary
//! serves the callee's INFO on its own. Each copy made a compared write the
//! other never read, so they diverged (ADR-0055 §7), and the call ends
//! cleanly from the union of both once a node meets the other's copy: the
//! copy, holding the round's client transaction, answers the round as its
//! own teardown does (RFC 3261 §15.1.2), and every dialog is BYEd.
//!
//! The axis is how the copy's write is held from the primary: a cut of both
//! directions, healed after the primary's INFO; or a stall of the copy's
//! writes alone, the primary's INFO reaching the copy first. Each cell
//! settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use scenario_harness::{InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::crossed_reinvite_round::{info_crossing_the_end, info_through_the_primary};
use crate::diverged_end::{accept_the_clean_end, answer_the_clean_end};

/// The proxy reads the primary dead; the caller's re-INVITE reaches the
/// backup's copy, which relays it to the callee, who holds its answer.
async fn open_the_round_at_the_copy(est: &mut Established) -> (InDialogTxn, ServerTxn) {
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    (round, at_bob)
}

/// The clean end reaches the round the copy holds: the copy answers the
/// caller's re-INVITE 487 and CANCELs it toward the callee once the callee's
/// provisional arrived (RFC 3261 §9.1); the callee refuses it 487. Every
/// dialog is BYEd and the call settles.
async fn the_round_ends_with_the_call(
    est: &mut Established,
    mut round: InDialogTxn,
    mut at_bob: ServerTxn,
) {
    round.expect_tolerating(487, &["BYE"]).await;
    at_bob.respond(100, "Trying").await;
    est.bob.receive_tolerating("CANCEL", &["BYE"]).await.respond(200, "OK").await;
    at_bob.respond(487, "Request Terminated").await;
    answer_the_clean_end(est).await;
}

/// The replication is cut both ways before the copy relays the round. The
/// primary, read alive again, relays the callee's INFO; the cut heals.
#[tokio::test(start_paused = true)]
async fn a_round_opened_across_a_cut_ends_with_the_call_once_healed() {
    let mut est = establish("round-opened-across-a-cut").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    let (round, at_bob) = open_the_round_at_the_copy(&mut est).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    accept_the_clean_end(&mut est);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    the_round_ends_with_the_call(&mut est, round, at_bob).await;
}

/// Only the copy's writes toward the primary are held. The primary, read
/// alive again, relays the callee's INFO, whose write reaches the copy first:
/// the copy meets it and ends the call.
#[tokio::test(start_paused = true)]
async fn a_round_held_one_way_behind_the_primarys_write_ends_with_the_call() {
    let mut est = establish("round-held-one-way").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    let (round, at_bob) = open_the_round_at_the_copy(&mut est).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    accept_the_clean_end(&mut est);
    info_crossing_the_end(&est.alice, &mut est.bob_dialog).await;
    est.fh.resume(&b_ord, &p_ord);
    the_round_ends_with_the_call(&mut est, round, at_bob).await;
}

/// The replication is held both ways and the copy serves the caller's whole
/// round while the primary is read dead; then the primary, read alive,
/// serves the callee's whole round on the caller's dialog. Released, each
/// node reads the other's round it never had: the call ends.
#[tokio::test(start_paused = true)]
async fn a_round_served_at_the_copy_across_the_primarys_own_round_ends_the_call() {
    let mut est = establish("round-crossing-the-primarys-round").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    est.fh.stall(&p_ord, &b_ord);
    let (mut round, mut at_bob) = open_the_round_at_the_copy(&mut est).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    let mut theirs = est.bob_dialog.request(InDialogMethod::Invite, Some(ANSWER)).await;
    est.alice.receive("INVITE").await.respond(200, "OK").with_sdp(OFFER).await;
    theirs.expect(200).await;
    est.bob_dialog.ack(None).await;
    est.alice.receive("ACK").await;
    accept_the_clean_end(&mut est);
    est.fh.resume(&b_ord, &p_ord);
    est.fh.resume(&p_ord, &b_ord);
    let (caller, callee) = answer_the_clean_end(&mut est).await;
    assert!(caller.byes > 0 && callee.byes > 0, "a BYE ends each dialog");
}

/// The replication is cut both ways and the copy serves the whole round —
/// answered, the caller's ACK relayed — while the primary is read dead; then
/// the primary, read alive, relays the callee's INFO and the cut heals. The
/// call ends, the callee's BYE following the round's CSeq toward it (RFC
/// 3261 §12.2.1.1).
#[tokio::test(start_paused = true)]
async fn a_round_served_whole_at_the_copy_ends_the_call_past_its_sequence() {
    let mut est = establish("round-served-whole-at-the-copy").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    let (mut round, mut at_bob) = open_the_round_at_the_copy(&mut est).await;
    let round_cseq = at_bob.request().cseq().seq();
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    accept_the_clean_end(&mut est);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    let (_, callee) = answer_the_clean_end(&mut est).await;
    let first = callee.first_bye_cseq.expect("a BYE ends the callee's dialog");
    assert!(first > round_cseq, "the callee's BYE follows the round's CSeq {round_cseq}");
}
