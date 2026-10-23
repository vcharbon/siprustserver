//! **A copy's write carrying a counter it adopted without the body that
//! carried it.** The proxy misroutes a request to the backup while the
//! primary lives (ADR-0014). Only the copy's writes toward the primary are
//! held: the primary serves the call on its own, and the copy takes the
//! primary's counter without the primary's body — refusing the primary's
//! write (ADR-0031 D3), or folding only the primary's re-INVITE round. Once
//! released, the copy's writes name the primary's counter, but the primary
//! never takes them whole: it takes only the progress they carry and keeps
//! its own writes:
//!
//! - the dialog sequence the primary's INFO advanced toward the callee
//!   (RFC 3261 §12.2.1.1);
//! - the INFO it relays, still awaiting the callee's answer;
//! - an UPDATE it saw answered, which stays answered (RFC 3311 §5.2);
//! - an INFO it relays when the copy took its round alone.
//!
//! Every cell ends the call with a BYE and settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use scenario_harness::{InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{
    crash_primary, establish, reboot_and_reclaim, Established, ANSWER, OFFER,
};
use crate::crossed_reinvite_round::{caller_hangs_up, info_through_the_primary};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The round the copy relays, read by the primary; the copy's writes toward
/// the primary then held; the callee's INFO relayed by the copy. The proxy
/// reads the primary dead.
async fn round_read_then_copy_held(est: &mut Established) -> (InDialogTxn, ServerTxn) {
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.fh.stall(&b_ord, &p_ord);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    (round, at_bob)
}

/// The callee answers the round at the copy, the proxy reading the primary
/// dead, and the caller's ACK reaches the callee through the copy.
async fn serve_the_round_at_the_copy(
    est: &mut Established,
    mut round: InDialogTxn,
    mut at_bob: ServerTxn,
) {
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(100)).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = round.expect(200).await;
    est.dialog.ack_for(answer.cseq().seq(), None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
}

/// The copy's writes are released and read; the proxy reads the primary
/// alive.
async fn release_the_copy(est: &mut Established) {
    est.fh.resume(&est.bak_ord.clone(), &est.primary_ord.clone());
    est.fh.advance(SETTLE).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
}

/// The primary relays a whole INFO from the caller while the copy holds the
/// round open; the copy then serves the round. The caller's BYE through the
/// primary follows that INFO's CSeq toward the callee.
#[tokio::test(start_paused = true)]
async fn the_sequence_the_primary_advanced_outlives_the_copys_adopted_counter() {
    let mut est = establish("adopted-counter-sequence").await;
    let (round, at_bob) = round_read_then_copy_held(&mut est).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    est.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    serve_the_round_at_the_copy(&mut est, round, at_bob).await;
    release_the_copy(&mut est).await;
    caller_hangs_up(&mut est).await;
}

/// The primary relays the caller's INFO and the callee holds its answer while
/// the copy serves the round, until the copy's writes are released. The INFO
/// is still the primary's to answer: the callee's 200 reaches the caller.
#[tokio::test(start_paused = true)]
async fn the_info_the_primary_holds_open_outlives_the_copys_adopted_counter() {
    let mut est = establish("adopted-counter-info-pending").await;
    let (round, at_bob) = round_read_then_copy_held(&mut est).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_bob = est.bob.receive("INFO").await;
    serve_the_round_at_the_copy(&mut est, round, at_bob).await;
    release_the_copy(&mut est).await;
    info_at_bob.respond(200, "OK").await;
    info.expect(200).await;
    caller_hangs_up(&mut est).await;
}

/// The primary relays the caller's UPDATE, which both nodes read; then the
/// copy's writes are held, and it relays the callee's INFO. The callee's 200
/// to the UPDATE reaches the primary, which closes the relay; the copy refuses
/// that write and adopts its counter, and relays a second INFO. Once the copy
/// is released, the UPDATE is still answered at the primary: the caller's
/// next UPDATE reaches the callee.
#[tokio::test(start_paused = true)]
async fn an_update_the_primary_saw_answered_stays_answered_past_the_copys_adopted_counter() {
    let mut est = establish("adopted-counter-update-answered").await;
    update_answered_at_the_primary_alone(&mut est).await;
    next_update_and_hang_up(&mut est).await;
}

/// The same, then the primary crashes and reboots onto what replication
/// holds of the call (ADR-0047 D7): the backup's Element holds the primary's
/// record, the UPDATE answered, so the caller's next UPDATE reaches the
/// callee.
#[tokio::test(start_paused = true)]
async fn an_update_the_primary_saw_answered_stays_answered_past_its_reboot() {
    let mut est = establish("adopted-counter-update-reboot").await;
    update_answered_at_the_primary_alone(&mut est).await;
    let Established { fh, proxy, w_b1, w_b2, primary_ord, .. } = &mut est;
    let (primary, backup) = if primary_ord == "b1" { (w_b1, &*w_b2) } else { (w_b2, &*w_b1) };
    crash_primary(primary, backup, primary_ord, proxy);
    fh.advance(Duration::from_millis(300)).await;
    reboot_and_reclaim(fh, primary, backup, primary_ord, proxy).await;
    next_update_and_hang_up(&mut est).await;
}

/// The caller's next UPDATE reaches the callee, and the caller hangs up.
async fn next_update_and_hang_up(est: &mut Established) {
    let mut next = est.dialog.request(InDialogMethod::Update, None).await;
    est.bob.receive("UPDATE").await.respond(200, "OK").await;
    next.expect(200).await;
    caller_hangs_up(est).await;
}

/// The caller's UPDATE, which both nodes read; the copy's writes held while
/// it relays the callee's INFO; the UPDATE's 200 through the primary; a
/// second INFO through the copy; the copy's writes released.
async fn update_answered_at_the_primary_alone(est: &mut Established) {
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    let mut update = est.dialog.request(InDialogMethod::Update, None).await;
    let mut update_at_bob = est.bob.receive("UPDATE").await;
    est.fh.advance(SETTLE).await;
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    update_at_bob.respond(200, "OK").await;
    update.expect(200).await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(Duration::from_millis(100)).await;
    release_the_copy(est).await;
    est.fh.advance(SETTLE).await;
}

/// The copy's writes are held, and it relays the callee's INFO. The primary
/// relays the caller's INFO, the callee holding its answer, then her
/// re-INVITE; the copy folds that round alone, which takes the primary's
/// counter, and relays a second INFO. Once the copy is released, the INFO is
/// still the primary's to answer: the callee's 200 reaches the caller.
#[tokio::test(start_paused = true)]
async fn an_info_the_primary_relays_outlives_a_round_the_copy_folded_alone() {
    let mut est = establish("adopted-counter-round-fold-info").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.fh.stall(&b_ord, &p_ord);
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    est.fh.advance(Duration::from_millis(100)).await;
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_bob = est.bob.receive("INFO").await;
    est.fh.advance(Duration::from_millis(100)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.fh.advance(Duration::from_millis(100)).await;
    release_the_copy(&mut est).await;

    info_at_bob.respond(200, "OK").await;
    info.expect(200).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = round.expect(200).await;
    est.dialog.ack_for(answer.cseq().seq(), None).await;
    est.bob.receive("ACK").await;
    caller_hangs_up(&mut est).await;
}
