//! **The dialog sequence each node advances while the replication between
//! them is held.** The proxy misroutes a request to the backup while the
//! primary lives (ADR-0014): the copy relays the callee's INFO toward the
//! caller, while the primary relays the caller's INFO toward the callee. Each
//! node's write lacks the other's request, so the copies diverged (ADR-0055
//! §7): once the replication heals, the call ends cleanly, and the first BYE
//! each party takes follows the highest CSeq either node sent it in that
//! dialog (RFC 3261 §12.2.1.1), whichever way the writes cross.

use std::time::Duration;

use failover_harness::WorkerHealth;
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::diverged_end::{accept_the_clean_end, answer_the_clean_end, Ended};

/// The caller sends an INFO; the callee answers it 200. Returns the CSeq it
/// reached the callee with.
async fn caller_info(est: &mut Established) -> u32 {
    let mut info = est.dialog.request(InDialogMethod::Info, None).await;
    let mut at_bob = est.bob.receive("INFO").await;
    let cseq = at_bob.request().cseq().seq();
    at_bob.respond(200, "OK").await;
    info.expect(200).await;
    cseq
}

/// The callee sends an INFO through the copy; the caller answers it 200.
/// Returns the CSeq it reached the caller with.
async fn callee_info(est: &mut Established) -> u32 {
    let mut info = est.bob_dialog.request(InDialogMethod::Info, None).await;
    let mut at_alice = est.alice.receive("INFO").await;
    let cseq = at_alice.request().cseq().seq();
    at_alice.respond(200, "OK").await;
    info.expect(200).await;
    cseq
}

/// The cut heals; the call ends cleanly, the first BYE each party takes past
/// `toward_caller` and `toward_callee`, the highest CSeq either node sent it.
async fn heal_and_end(est: &mut Established, toward_caller: u32, toward_callee: u32) {
    accept_the_clean_end(est);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    let (caller, callee) = answer_the_clean_end(est).await;
    let first = |e: Ended| e.first_bye_cseq.expect("a BYE ends the dialog");
    assert!(first(caller) > toward_caller, "the caller's BYE follows {toward_caller}");
    assert!(first(callee) > toward_callee, "the callee's BYE follows {toward_callee}");
}

/// The replication is cut both ways. The copy relays the callee's INFO while
/// the proxy reads the primary dead; the primary, read alive again, relays
/// the caller's INFO; then the cut heals.
#[tokio::test(start_paused = true)]
async fn each_nodes_sequence_outlives_the_split_into_the_clean_end() {
    let mut est = establish("split-sequence-refused").await;
    est.fh.partition(&est.primary_ord, &est.bak_ord);
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let toward_caller = callee_info(&mut est).await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    let toward_callee = caller_info(&mut est).await;
    heal_and_end(&mut est, toward_caller, toward_callee).await;
}

/// The copy relays the caller's re-INVITE and the primary reads it, holding
/// the round open; then the replication is cut both ways. The copy relays the
/// callee's INFO and serves the round to its ACK, while the primary relays
/// the caller's INFO; then the cut heals.
#[tokio::test(start_paused = true)]
async fn a_round_served_at_the_copy_and_the_primarys_info_end_past_both_sequences() {
    let mut est = establish("split-sequence-round").await;
    let (p_ord, b_ord) = (est.primary_ord.clone(), est.bak_ord.clone());
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    est.fh.partition(&p_ord, &b_ord);
    let toward_caller = callee_info(&mut est).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    let info_toward_callee = caller_info(&mut est).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(100)).await;
    let round_toward_callee = at_bob.request().cseq().seq();
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    let answer = round.expect(200).await;
    est.dialog.ack_for(answer.cseq().seq(), None).await;
    est.bob.receive("ACK").await;
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    heal_and_end(&mut est, toward_caller, info_toward_callee.max(round_toward_callee)).await;
}
