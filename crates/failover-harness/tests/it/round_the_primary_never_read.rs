//! **A re-INVITE round the backup's copy opens, which its primary has not
//! read when it writes the call again.** The proxy misroutes the caller's
//! re-INVITE to the backup while the primary lives (ADR-0014); the copy
//! relays it, but its write does not reach the primary before the primary
//! serves the callee's INFO on its own. The round's opening is progress the
//! primary cannot make itself (ADR-0031, "a re-INVITE round is a third
//! chain"): once the replication heals, the primary's record holds the round,
//! and the copy, holding the round's client transaction, stands down only on
//! a primary's write that carries it.
//!
//! The axis is how the copy's write is held from the primary: a cut of both
//! directions, healed after the primary's INFO; or a stall of the copy's
//! writes alone, the primary's INFO reaching the copy first. Each cell then
//! has the callee answer the round, the caller ACK it through the primary
//! (RFC 3261 §13.2.2.4: the callee draws one ACK), and ends the call with a
//! BYE that settles to one CDR.

use std::time::Duration;

use failover_harness::{ReplicatedB2buaSut, WorkerHealth};
use scenario_harness::{InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::crossed_reinvite_round::{caller_hangs_up, info_through_the_primary};
use crate::reinvite_2xx_acked_through_the_primary::ack_the_answer;

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

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

fn backup(est: &Established) -> &ReplicatedB2buaSut {
    if est.primary_ord == "b1" {
        &est.w_b2
    } else {
        &est.w_b1
    }
}

/// The healed replication has settled: the copy still serves the round, no
/// write of the primary's having carried it yet. Then the primary relays a
/// second INFO from the callee, a write that carries the round, and the copy
/// stands down on it.
async fn primary_writes_the_round(est: &mut Established) {
    assert!(backup(est).serves(&est.call_ref), "the copy keeps the round it holds alone");
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(SETTLE).await;
    assert!(!backup(est).serves(&est.call_ref), "the copy stands down on the primary's write");
    assert_eq!(backup(est).metrics().repl_stand_down_total(), 1);
}

/// The callee answers the round; the caller ACKs through the primary and
/// re-ACKs every copy of the 2xx; the caller hangs up.
async fn answer_and_end(mut est: Established, mut round: InDialogTxn, mut at_bob: ServerTxn) {
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

/// The replication is cut both ways before the copy relays the round. The
/// primary, read alive again, relays the callee's INFO; the cut heals; the
/// primary relays a second INFO, a write the copy reads.
#[tokio::test(start_paused = true)]
async fn a_round_opened_across_a_cut_completes_once_healed() {
    let mut est = establish("round-opened-across-a-cut").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    let (round, at_bob) = open_the_round_at_the_copy(&mut est).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;
    primary_writes_the_round(&mut est).await;
    answer_and_end(est, round, at_bob).await;
}

/// Only the copy's writes toward the primary are held. The primary, read
/// alive again, relays the callee's INFO, whose write reaches the copy first;
/// the copy's write is released; the primary relays a second INFO.
#[tokio::test(start_paused = true)]
async fn a_round_held_one_way_behind_the_primarys_write_completes() {
    let mut est = establish("round-held-one-way").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    let (round, at_bob) = open_the_round_at_the_copy(&mut est).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.fh.resume(&b_ord, &p_ord);
    est.fh.advance(SETTLE).await;
    primary_writes_the_round(&mut est).await;
    answer_and_end(est, round, at_bob).await;
}

/// The replication is cut both ways and the copy serves the caller's whole
/// round while the primary is read dead; then the primary, read alive,
/// serves the callee's whole round on the caller's dialog, and the cut
/// heals. Each round is the other node's progress on its own dialog: the
/// primary's record holds the copy's round, its transaction and its ACK, so a
/// repeat of the callee's 2xx is re-ACKed from it (RFC 3261 §13.2.2.4).
#[tokio::test(start_paused = true)]
async fn a_round_served_at_the_copy_across_the_primarys_own_round_reaches_the_primary() {
    let mut est = establish("round-crossing-the-primarys-round").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    let (mut round, mut at_bob) = open_the_round_at_the_copy(&mut est).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    let copys = backup(&est).live_call(&est.call_ref).expect("the copy serves the call");
    let callee_txn =
        |c: &call::Call| c.b_legs[0].dialogs.confirmed().and_then(|d| d.pending_invite_txn.clone());
    let served = callee_txn(&copys).expect("the copy's round");
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    let mut theirs = est.bob_dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    est.alice.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    theirs.expect(200).await;
    est.bob_dialog.ack(None).await;
    est.alice.receive("ACK").await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;

    let primary = if est.primary_ord == "b1" { &est.w_b1 } else { &est.w_b2 };
    let record = primary.live_call(&est.call_ref).expect("the primary serves the call");
    assert_eq!(callee_txn(&record), Some(served), "the primary's record holds the copy's round");
    caller_hangs_up(&mut est).await;
}

/// The replication is cut both ways and the copy serves the whole round —
/// answered, the caller's ACK relayed — while the primary is read dead; then
/// the primary, read alive, relays the callee's INFO and the cut heals. The
/// caller's BYE through the primary follows the round's CSeq toward the
/// callee (RFC 3261 §12.2.1.1).
#[tokio::test(start_paused = true)]
async fn a_round_served_whole_at_the_copy_moves_the_primarys_dialog_sequence() {
    let mut est = establish("round-served-whole-at-the-copy").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    let (mut round, mut at_bob) = open_the_round_at_the_copy(&mut est).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;
    caller_hangs_up(&mut est).await;
}
