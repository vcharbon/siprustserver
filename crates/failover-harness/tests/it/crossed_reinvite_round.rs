//! **A re-INVITE round the backup's copy serves, crossing the primary's own
//! writes.** The proxy misroutes the caller's re-INVITE to the backup while
//! the primary lives (ADR-0014); the copy relays it, and the primary reads the
//! relay. Then the replication between the two nodes is cut: the primary
//! serves the callee's INFO on its own while the copy takes the round's final,
//! so neither write reaches the other node until the cut heals. Each copy made
//! a compared write the other never read, so neither contains the other
//! (ADR-0055 §7): once healed, the call ends cleanly from the union of both —
//! every dialog BYEd at the higher of both CSeqs, one CDR — whatever the
//! round reached at the copy and whatever the primary holds of its INFO.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use scenario_harness::{Agent, Dialog, InDialogTxn, ServerTxn};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::diverged_end::{accept_the_clean_end, answer_the_clean_end};

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
pub(crate) async fn info_through_the_primary(alice: &Agent, bob_dialog: &mut Dialog) {
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
}

/// The callee sends an INFO through the primary; the caller answers it 200.
/// A BYE of the clean end of two diverged copies may cross it, at either
/// party, and is answered as it comes.
pub(crate) async fn info_crossing_the_end(alice: &Agent, bob_dialog: &mut Dialog) {
    let mut info = bob_dialog.request(InDialogMethod::Info, None).await;
    alice.receive_tolerating("INFO", &["BYE"]).await.respond(200, "OK").await;
    info.expect_tolerating(200, &["BYE"]).await;
}

/// The caller hangs up and the call settles: nothing of it is left anywhere,
/// one CDR.
pub(crate) async fn caller_hangs_up(est: &mut Established) {
    let mut bye = est.dialog.bye().await;
    est.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
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

/// The cut heals: the two copies meet, diverged, and the call ends cleanly,
/// a BYE ending each party's dialog.
async fn heal_and_end(est: &mut Established) {
    accept_the_clean_end(est);
    est.fh.heal(&est.primary_ord, &est.bak_ord);
    let (caller, callee) = answer_the_clean_end(est).await;
    assert!(caller.byes > 0 && callee.byes > 0, "a BYE ends each dialog");
}

/// The primary relays the callee's INFO and the caller holds it while the copy
/// relays the round's 2xx. The caller answers the INFO and ACKs the 2xx
/// before the heal; once healed the call ends.
#[tokio::test(start_paused = true)]
async fn a_crossed_answered_round_with_an_info_the_primary_holds_ends_the_call() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-info-pending").await;
    let mut info = est.bob_dialog.request(InDialogMethod::Info, None).await;
    let mut info_at_alice = est.alice.receive("INFO").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    info_at_alice.respond(200, "OK").await;
    info.expect(200).await;
    heal_and_end(&mut est).await;
}

/// The primary relays a whole INFO from the callee while the copy relays the
/// round's 2xx: once healed the call ends, its BYEs past both copies'
/// sequences (RFC 3261 §12.2.1.1).
#[tokio::test(start_paused = true)]
async fn a_crossed_answered_round_ends_the_call_past_the_sequence_the_primary_advanced() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-info-done").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    heal_and_end(&mut est).await;
}

/// The primary relays a whole INFO while the copy relays the callee's 488 to
/// the round: once healed the call ends.
#[tokio::test(start_paused = true)]
async fn a_crossed_refused_round_ends_the_call() {
    let Crossed { mut est, mut round, mut at_bob } = cross("crossed-round-refused").await;
    info_through_the_primary(&est.alice, &mut est.bob_dialog).await;
    at_bob.respond(488, "Not Acceptable Here").await;
    round.expect(488).await;
    heal_and_end(&mut est).await;
}

/// The primary relays a whole INFO while the copy serves the round to its
/// end — answered, the caller's ACK relayed — and relays the caller's next
/// re-INVITE, which the callee answers once healed: the call ends, the next
/// round's 2xx ACKed by the caller.
#[tokio::test(start_paused = true)]
async fn a_round_ended_at_the_copy_with_the_next_round_open_ends_the_call() {
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
    next_at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    next.expect(200).await;
    est.dialog.ack(None).await;
    est.bob.receive("ACK").await;
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    heal_and_end(&mut est).await;
}

/// The copy relays the round's 2xx and the primary reads it, so the primary
/// awaits the caller's ACK. Then the replication is cut: the primary relays a
/// whole INFO while the copy relays the caller's ACK. Once healed the call
/// ends.
#[tokio::test(start_paused = true)]
async fn an_answered_round_acked_at_the_copy_across_the_primarys_info_ends_the_call() {
    let mut est = establish("crossed-round-answered-then-acked").await;
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
    est.proxy.set_health(&p_ord, WorkerHealth::Alive);
    heal_and_end(&mut est).await;
}
