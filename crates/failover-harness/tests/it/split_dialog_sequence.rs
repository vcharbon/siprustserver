//! **The dialog sequence each node advances while the replication between
//! them is held.** The proxy misroutes a request to the backup while the
//! primary lives (ADR-0014): the copy relays the callee's INFO toward the
//! caller, while the primary relays the caller's INFO toward the callee. Each
//! node's write lacks the other's request, so neither replaces the other's
//! record, and each advanced a dialog sequence the other did not see. Once
//! the replication heals, the primary's next request on either dialog
//! follows the highest CSeq either node sent there (RFC 3261 §12.2.1.1),
//! whichever way the writes cross:
//!
//! - the copy's write is refused at the primary and its counter adopted
//!   (ADR-0031 D3), so the copy stands down on the primary's next write;
//! - the copy's write ends a re-INVITE round the primary holds open, so the
//!   round's end folds alone into the primary's record (ADR-0031, "a round's
//!   end folds alone").
//!
//! Every cell ends the call with the callee's BYE through the primary and
//! settles to one CDR.

use std::time::Duration;

use failover_harness::WorkerHealth;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::crossed_reinvite_round::{callee_hangs_up, info_through_the_primary};

/// How long the replication between the two nodes takes to settle.
const SETTLE: Duration = Duration::from_millis(500);

/// The caller sends an INFO; the callee answers it 200.
async fn caller_info(est: &mut Established) {
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    est.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
}

/// The replication is cut both ways. The copy relays the callee's INFO while
/// the proxy reads the primary dead; the primary, read alive again, relays
/// the caller's INFO; then the cut heals.
#[tokio::test(start_paused = true)]
async fn a_refused_copys_sequence_toward_the_caller_outlives_its_stand_down() {
    let mut est = establish("split-sequence-refused").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    caller_info(&mut est).await;
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    est.fh.advance(SETTLE).await;
    caller_info(&mut est).await;
    est.fh.advance(SETTLE).await;
    callee_hangs_up(&mut est).await;
}

/// The copy relays the caller's re-INVITE and the primary reads it, holding
/// the round open; then the replication is cut both ways. The copy relays the
/// callee's INFO and serves the round to its ACK, while the primary relays
/// the caller's INFO; then the cut heals, and the primary reads the copy's
/// last write alone.
#[tokio::test(start_paused = true)]
async fn a_round_folded_alone_keeps_the_copys_sequence_toward_the_caller() {
    let mut est = establish("split-sequence-round-fold").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.fh.partition(&p_ord, &b_ord);
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    caller_info(&mut est).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(100)).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = round.expect(200).await;
    est.dialog.ack_for(answer.cseq().seq(), None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.fh.heal(&p_ord, &b_ord);
    est.fh.advance(SETTLE).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    callee_hangs_up(&mut est).await;
}
