//! **A re-INVITE 2xx a copy relayed, ACKed at a primary that does not hold the
//! copy's write.** The copy's replication toward the live primary stalls, the
//! proxy misroutes the caller's re-INVITE to the copy, then reads the primary
//! alive: the callee's 2xx follows the copy's Via, the caller's ACK reaches the
//! primary, whose record lacks the round and reads as a call no other owner
//! served. The primary drops the ACK and counts it (ADR-0055 §4, trigger 3): the
//! copy that answered is live and its write is on its way.
//!
//! Two outcomes, by whether the write ever arrives:
//! - it does: a copy of the 2xx draws her ACK again (RFC 3261 §13.2.2.4), which
//!   the primary now relays, and the callee draws exactly one ACK;
//! - it does not (a split): at 64·T1 the copy either gives up on the ACK, ACKing
//!   the callee's 2xx and BYEing both parties, or sheds the call as the
//!   transactions it served clear, whichever of the two falls first, and then
//!   the callee, its 2xx never ACKed, BYEs the dialog itself (RFC 3261 §13.3.1.4).
//!   The primary, which never read the round, ends the call on that BYE, or else
//!   at its next keepalive, which the 481s of the ended dialogs end. No call is
//!   left held, and one CDR is written.
//!
//! Beyond the failover sweep: the proxy misroutes one round of a call, over a
//! one-way stall.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, Incident, WorkerHealth};
use scenario_harness::{Agent, Inbound};
use sip_message::generators::InDialogMethod;
use sip_message::{Method, SipResponse};
use tokio::time::Instant;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::reinvite_2xx_acked_through_the_primary::{ack_the_answer, re_ack_copies};

/// The bound by which the copy has ended its part, from the 2xx: the 64·T1 of
/// RFC 3261 §13.3.1.4, then the outbox's cap and the transit.
const COPY_GIVES_UP: Duration = Duration::from_secs(33);

/// The bound by which the primary ends the call its parties left, from the
/// copy's end: its next keepalive, one interval, and the turn the 481 draws.
const PRIMARY_ENDS: Duration = Duration::from_secs(301);

/// The clock step the parties read their sockets at.
const STEP: Duration = Duration::from_millis(100);

/// A copy of the re-INVITE's 2xx (RFC 3261 §13.3.1.4).
fn copy_of_the_answer(r: &SipResponse) -> bool {
    r.status() == 200 && *r.cseq().method() == Method::Invite
}

/// Stall the copy's writes, misroute the round to it, read the primary alive
/// again, and let the callee's 2xx reach the caller. Returns the round's CSeq.
async fn the_copy_relays_a_2xx(est: &mut Established) -> u32 {
    est.proxy.tell_no_dead_verdict();
    est.fh.stall(&est.bak_ord, &est.primary_ord);
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Dead);
    est.fh.advance(Duration::from_millis(200)).await;
    let mut round = est.dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = est.bob.receive("INVITE").await;
    let round_cseq = at_bob.request().cseq().seq();
    round.expect(100).await;
    est.fh.advance(Duration::from_millis(300)).await;
    let backup = if est.primary_ord == "b1" { &est.w_b2 } else { &est.w_b1 };
    assert!(backup.serves(&est.call_ref), "the copy serves the round");
    est.proxy.set_health(&est.primary_ord, WorkerHealth::Alive);
    est.fh.advance(Duration::from_millis(200)).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect(200).await;
    round_cseq
}

/// How many ACKs the primary dropped.
fn dropped_at_the_primary(est: &Established) -> u64 {
    let primary = if est.primary_ord == "b1" { &est.w_b1 } else { &est.w_b2 };
    primary.metrics().unexplained_acks_total()
}

async fn settled(est: &Established) {
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

#[tokio::test(start_paused = true)]
async fn the_write_arriving_after_her_ack_was_dropped_leaves_the_callee_one_ack() {
    let mut est = establish("reinvite-2xx-acked-copy-stalled-then-healed").await;
    let round_cseq = the_copy_relays_a_2xx(&mut est).await;

    let before_heal =
        ack_the_answer(&est.fh, &est.alice, &mut est.dialog, &est.bob, round_cseq).await;
    assert!(dropped_at_the_primary(&est) >= 1, "the primary dropped her ACK and counted it");
    est.fh.heal(&est.bak_ord, &est.primary_ord);
    let after_heal =
        re_ack_copies(&est.fh, &est.alice, &mut est.dialog, &est.bob, round_cseq).await;
    assert_eq!(before_heal + after_heal, 1, "the callee draws exactly one ACK for its 2xx");

    let mut bye = est.dialog.bye().await;
    est.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settled(&est).await;
}

/// What a party does with the requests that reach it: a BYE of the dialog it
/// still holds is answered 200 and ends it, anything on a dialog `ended` is
/// answered 481 (RFC 3261 §12.2.2), and an ACK draws nothing. Returns their
/// methods.
async fn answers_as_its_dialog_stands(party: &Agent, ended: &mut bool) -> Vec<Method> {
    let mut seen = Vec::new();
    while let Some(inbound) = party.take_queued_inbound().await {
        let Inbound::Request(mut request) = inbound else { continue };
        let method = request.request().method().clone();
        seen.push(method.clone());
        match method {
            Method::Ack => {}
            Method::Bye if !*ended => {
                *ended = true;
                request.respond(200, "OK").await;
            }
            _ => request.respond(481, "Call/Transaction Does Not Exist").await,
        }
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn a_write_that_never_arrives_ends_the_call_within_a_keepalive() {
    let mut est = establish("reinvite-2xx-acked-copy-stalled-never-healed").await;
    let round_cseq = the_copy_relays_a_2xx(&mut est).await;
    est.fh.strike(Incident::Split);
    est.dialog.ack(None).await;

    // Each copy of the 2xx draws her ACK to the primary, until the copy's part
    // ends at 64·T1.
    let (mut alice_ended, mut bob_ended) = (false, false);
    let mut acked = 0;
    let answered = Instant::now();
    while !(alice_ended && bob_ended) && answered.elapsed() < COPY_GIVES_UP {
        est.fh.advance(STEP).await;
        while let Some(inbound) = est.alice.take_queued_inbound().await {
            match inbound {
                Inbound::Response(r) if copy_of_the_answer(&r) => est.dialog.ack(None).await,
                Inbound::Request(mut bye) if bye.request().method() == "BYE" => {
                    alice_ended = true;
                    bye.respond(200, "OK").await;
                }
                Inbound::Request(r) => {
                    panic!("unexpected {} toward the caller", r.request().method())
                }
                Inbound::Response(r) => panic!("unexpected {} toward the caller", r.status()),
            }
        }
        let seen = answers_as_its_dialog_stands(&est.bob, &mut bob_ended).await;
        acked += seen.iter().filter(|m| **m == Method::Ack).count();
    }
    assert_eq!(alice_ended, bob_ended, "the copy BYEs both parties or neither");
    assert_eq!(acked, usize::from(bob_ended), "the copy's ACK of the 2xx at {round_cseq}");
    assert!(dropped_at_the_primary(&est) >= 1, "the primary dropped her ACKs and counted them");

    // The copy shed the call: the callee, its 2xx never ACKed, BYEs the dialog.
    if !bob_ended {
        (alice_ended, bob_ended) = (true, true);
        let mut bye = est.bob_dialog.bye().await;
        est.alice.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
    }

    // The primary ends the call on that BYE, or at a keepalive into ended dialogs.
    let primary = if est.primary_ord == "b1" { &est.w_b1 } else { &est.w_b2 };
    let copy_done = Instant::now();
    while primary.serves(&est.call_ref) {
        assert!(copy_done.elapsed() < PRIMARY_ENDS, "the primary ends it by {PRIMARY_ENDS:?}");
        est.fh.advance(STEP).await;
        answers_as_its_dialog_stands(&est.alice, &mut alice_ended).await;
        answers_as_its_dialog_stands(&est.bob, &mut bob_ended).await;
    }
    settled(&est).await;
}
