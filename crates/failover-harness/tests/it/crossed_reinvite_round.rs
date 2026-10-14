//! **A re-INVITE round the backup's copy serves, crossing the primary's own
//! writes.** The proxy misroutes the caller's re-INVITE to the backup while
//! the primary lives (ADR-0014); the copy relays it, and the primary reads the
//! relay. Then the replication between the two nodes is cut: the primary
//! serves the callee's INFO on its own while the copy takes the round's final,
//! so neither write reaches the other node until the cut heals. Only the copy,
//! which holds the round's client transaction, sees the final; only the
//! primary saw the INFO. Once healed, each node keeps what the other could not
//! make (ADR-0031, "a re-INVITE round is a third chain"):
//!
//! - the primary keeps its INFO, pending or completed, and the dialog sequence
//!   the INFO advanced (RFC 3261 §12.2.1.1);
//! - the round ends at the primary as it ended at the copy: answered, its ACK
//!   relayed once (§13.2.2.4); refused, its dialog free for the next round;
//!   followed by a next round, that round held open.
//!
//! Every cell ends the call with a BYE and settles to one CDR.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use scenario_harness::{Agent, Dialog, InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::reinvite_2xx_acked_through_the_primary::{ack_the_answer, re_ack_copies};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The call with the caller's re-INVITE round open at the copy, the primary
/// holding the relay, and the replication between the two nodes cut; the
/// proxy reads the primary alive.
struct Crossed {
    est: Established,
    round: InDialogTxn,
    at_bob: ServerTxn,
}

async fn cross(name: &str) -> Crossed {
    let mut est = establish(name).await;
    let backup = if est.primary_ord == "b1" { &est.w_b2 } else { &est.w_b1 };
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    assert!(backup.serves(&est.call_ref), "the backup's copy serves the round");
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    Crossed { est, round, at_bob }
}

/// The callee sends an INFO through the primary; the caller answers it 200.
async fn info_through_the_primary(alice: &Agent, bob_dialog: &mut Dialog) {
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
}

/// The caller hangs up and the call settles: nothing of it is left anywhere,
/// one CDR.
async fn caller_hangs_up(est: &mut Established) {
    let mut bye = est.dialog.bye().await;
    est.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle(est).await;
}

/// The callee hangs up through the primary and the call settles.
async fn callee_hangs_up(est: &mut Established) {
    let mut bye = est.bob_dialog.bye().await;
    est.alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle(est).await;
}

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

/// The primary relays the callee's INFO and the caller holds it while the copy
/// relays the round's 2xx. Once healed the INFO is still the primary's to
/// answer: the caller's 200 reaches the callee, and the callee draws one ACK
/// for its 2xx.
#[tokio::test(start_paused = true)]
async fn a_crossed_answered_round_keeps_the_info_the_primary_holds_open() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-info-pending").await;
    let mut info = est.bob_dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_alice = est.alice.receive("INFO").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;

    info_at_alice.respond(200, "OK").await;
    info.expect(200).await;
    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

/// The primary relays a whole INFO from the callee while the copy relays the
/// round's 2xx. Once healed the callee's BYE through the primary follows the
/// INFO's CSeq toward the caller (RFC 3261 §12.2.1.1), with no deviation
/// accepted.
#[tokio::test(start_paused = true)]
async fn a_crossed_answered_round_keeps_the_dialog_sequence_the_primary_advanced() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-info-done").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;

    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    callee_hangs_up(&mut est).await;
}

/// The primary relays a whole INFO while the copy relays the callee's 488 to
/// the round. Once healed the round is over at the primary too: the caller's
/// next re-INVITE through it reaches the callee.
#[tokio::test(start_paused = true)]
async fn a_crossed_refused_round_frees_the_dialog_for_the_next_round() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-refused").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    at_bob.respond(488, "Not Acceptable Here").await;
    round.expect(488).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(Duration::from_secs(2)).await;

    let mut next = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    est.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    next.expect(200).await;
    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

/// The primary relays a whole INFO while the copy serves the round to its
/// end — answered, the caller's ACK relayed — and relays the caller's next
/// re-INVITE; the primary reads only the copy's last write. Once healed the
/// first round is over at the primary and the next one open: the callee's
/// 2xx to it draws one ACK through the primary.
#[tokio::test(start_paused = true)]
async fn a_round_ended_in_one_drain_leaves_the_next_round_open_at_the_primary() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-next-round").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    let mut next = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut next_at_bob = est.bob.receive("INVITE").await;
    next.expect(100).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;

    next_at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    next.expect(200).await;
    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

/// The primary relays a whole INFO while the copy serves the round to its
/// end, the caller's ACK reaching the copy. Once healed the callee draws no
/// further ACK for its 2xx, whatever copies of it the caller re-ACKs.
#[tokio::test(start_paused = true)]
async fn a_round_acked_at_the_copy_draws_no_second_ack_through_the_primary() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-acked").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;

    let further = re_ack_copies(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(further, 0, "the callee draws no second ACK for its 2xx");
    caller_hangs_up(&mut est).await;
}

/// The copy relays the round's 2xx and the primary reads it, so the primary
/// awaits the caller's ACK. Then the replication is cut: the primary relays a
/// whole INFO while the copy relays the caller's ACK and her next re-INVITE.
/// Once healed the first round is over at the primary — no second ACK for its
/// 2xx — and the next one open: the callee's 2xx to it draws one ACK.
#[tokio::test(start_paused = true)]
async fn an_answered_round_followed_by_the_next_round_ends_at_the_primary() {
    let mut est = establish("crossed-round-answered-then-next").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = round.expect(200).await;
    est.fh.advance(Duration::from_millis(150)).await;
    est.fh.partition(&p_ord, &b_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.dialog.ack_for(answer.cseq().seq(), None).await;
    est.bob.receive("ACK").await;
    let mut next = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut next_at_bob = est.bob.receive("INVITE").await;
    next.expect(100).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.heal(&p_ord, &b_ord);
    est.fh.advance(Duration::from_millis(200)).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(Duration::from_millis(200)).await;

    next_at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    next.expect(200).await;
    let acks = ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob).await;
    assert_eq!(acks, 1, "the callee draws exactly one ACK for each 2xx");
    caller_hangs_up(&mut est).await;
}
