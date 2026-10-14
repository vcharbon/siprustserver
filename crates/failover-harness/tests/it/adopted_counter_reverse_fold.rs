//! **A copy's write carrying a counter it adopted without the body that
//! carried it.** The proxy misroutes the caller's re-INVITE to the backup
//! while the primary lives (ADR-0014); the copy relays it and the primary
//! reads the relay, holding the round open. Then only the copy's writes
//! toward the primary are held: the primary relays the caller's INFO toward
//! the callee on its own, and the copy refuses that write and adopts its
//! counter (ADR-0031 D3), while it relays the callee's INFO toward the caller
//! and serves the round to its ACK. Once released, the copy's writes name
//! the primary's counter, but its body lacks the INFO the primary relayed:
//! the primary takes the round's end from it and keeps its own writes
//! (ADR-0031, "a round's end folds alone"):
//!
//! - the dialog sequence the primary's INFO advanced toward the callee
//!   (RFC 3261 §12.2.1.1);
//! - the INFO it relays, still awaiting the callee's answer.
//!
//! Every cell ends the call with a BYE and settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use scenario_harness::{InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
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
