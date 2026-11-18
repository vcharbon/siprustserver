//! The server transaction's answers to copies of its request (RFC 3261
//! §17.2.1, §17.2.2, §9.2): the last provisional and the final replayed for an
//! INVITE copy, the response replayed for a non-INVITE copy and the 200 to a
//! CANCEL copy, each counted per trigger (top-Via branch and the copy's
//! method); an Accepted INVITE absorbs its copies (RFC 6026 §7.1); a CANCEL
//! matching no INVITE is answered through its own non-INVITE transaction.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::SipMessage;
use sip_retransmit::timers::T1;
use sip_retransmit::Class;
use sip_txn::{CopyKind, TransactionEvent, TxnSeed};

use super::{answers, burst, copies_every, COPIES};
use crate::common::*;

const TRANSIT: u64 = 5;

/// 64·T1: no copy later than this after its trigger's first copy is answered.
const WINDOW: u64 = 64 * T1;

/// The INVITE copies a compliant client re-sends on Timer A (RFC 3261
/// §17.1.1.2) before Timer B.
fn invite_resends() -> usize {
    usize::try_from(Class::InviteClient.resends()).expect("fits")
}

/// The INVITE `branch` opens, answered `status` by the TU; the wire and the
/// events drained.
async fn invite_answered(stack: &mut Stack, branch: &str, call_id: &str, status: u16) {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(20).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
    let resp = parse_response(&response_bytes(status, "Answer", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.expect("the layer runs");
    elapse_ms(20).await;
    assert_eq!(answers(&stack.drain_peer(), status, "INVITE"), 1, "the TU's {status} leaves");
}

/// The requests of `method` handed up in a drained batch.
fn handed_up(events: &[TransactionEvent], method: &str) -> Vec<sip_message::SipRequest> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A ringing INVITE copied at wire speed draws its 180 for the copies Timer A
/// re-sends and absorbs the rest; a copy T1 after the last answered one is
/// answered again, and one later than 64·T1 after the INVITE is not.
#[tokio::test(start_paused = true)]
async fn a_ringing_invite_copied_at_wire_speed_draws_its_180_on_timer_as_schedule() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-ringing-copies", "ringing-copies");
    invite_answered(&mut stack, branch, call_id, 180).await;
    let invite = inbound_request("INVITE", branch, call_id, None);

    burst(&stack, &invite, COPIES).await;
    elapse_ms(20).await;
    let replayed = answers(&stack.drain_peer(), 180, "INVITE");
    assert_eq!(replayed, invite_resends(), "the copies Timer A re-sends, then none inside T1");
    let absorbed = stack.txn.metrics().copies_past_schedule(CopyKind::Request);
    assert_eq!(absorbed, (COPIES - replayed) as u64, "each copy absorbed is counted");

    elapse_ms(T1).await;
    stack.inject(&invite).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack.drain_peer(), 180, "INVITE"), 1, "T1 after the last answered");

    elapse_ms(WINDOW).await;
    stack.inject(&invite).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack.drain_peer(), 180, "INVITE"), 0, "past 64·T1 of the INVITE");
    assert!(handed_up(&stack.drain_events(), "INVITE").is_empty(), "no copy goes up");
}

/// A seeded INVITE with nothing sent yet composes its 100 for the copy that
/// first reaches it, then answers the copies Timer A re-sends past that one
/// and absorbs the rest.
#[tokio::test(start_paused = true)]
async fn a_seeded_invite_copied_at_wire_speed_draws_its_100_on_timer_as_schedule() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seeded-copies", "seeded-copies");
    let seed = TxnSeed::ServerInvite {
        request: request_of(&inbound_request("INVITE", branch, call_id, None)),
        to_tag: Some("seed-to-tag".to_string()),
        provisional: None,
        leg_id: Some("a".to_string()),
    };
    assert_eq!(stack.txn.seed("w0|seeded-copies|atag", vec![seed]).await.unwrap(), 1);

    burst(&stack, &inbound_request("INVITE", branch, call_id, None), COPIES).await;
    elapse_ms(20).await;
    let trying = answers(&stack.drain_peer(), 100, "INVITE");
    assert_eq!(trying, 1 + invite_resends(), "the first arrival and Timer A's re-sends");
    assert!(stack.drain_events().is_empty(), "no copy goes up");
}

/// An INVITE answered with a non-2xx final, copied at wire speed and never
/// ACKed (Completed), or copied after its ACK (Confirmed): the final is
/// replayed for the copies Timer A re-sends, and the rest are absorbed.
#[tokio::test(start_paused = true)]
async fn a_rejected_invite_copied_at_wire_speed_draws_its_final_on_timer_as_schedule() {
    for acked in [false, true] {
        let mut stack = Stack::build(TRANSIT, 64, 64).await;
        let (branch, call_id) = ("z9hG4bK-rejected-copies", "rejected-copies");
        invite_answered(&mut stack, branch, call_id, 486).await;
        if acked {
            stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
            elapse_ms(20).await;
        }

        burst(&stack, &inbound_request("INVITE", branch, call_id, None), COPIES).await;
        elapse_ms(20).await;
        let replayed = answers(&stack.drain_peer(), 486, "INVITE");
        assert_eq!(replayed, invite_resends(), "ACKed {acked}: Timer A's re-sends only");
    }
}

/// An Accepted INVITE absorbs every copy (RFC 6026 §7.1): its 2xx is the
/// TU's to re-send.
#[tokio::test(start_paused = true)]
async fn an_accepted_invite_absorbs_its_copies() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-accepted-copies", "accepted-copies");
    invite_answered(&mut stack, branch, call_id, 200).await;

    burst(&stack, &inbound_request("INVITE", branch, call_id, None), COPIES).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(answers(&out, 200, "INVITE"), 0, "the copies draw no 2xx");
    assert_eq!(count_responses(&out, 100), 0, "and no 100");
}

/// An Accepted seed whose 2xx has not left sends it to the first copy, after
/// a takeover its first chance, and absorbs the copies after.
#[tokio::test(start_paused = true)]
async fn an_accepted_seed_sends_its_2xx_to_the_first_copy_only() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-accepted-seed-copies", "accepted-seed-copies");
    let seed = TxnSeed::ServerAccepted {
        response: parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true)),
        leg_id: Some("a".to_string()),
    };
    assert_eq!(stack.txn.seed("w0|accepted-seed|atag", vec![seed]).await.unwrap(), 1);

    burst(&stack, &inbound_request("INVITE", branch, call_id, None), COPIES).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack.drain_peer(), 200, "INVITE"), 1, "the seeded 2xx leaves once");
    assert!(stack.drain_events().is_empty(), "no copy goes up");
}

/// A non-INVITE request answered by the TU, copied at wire speed: its final
/// is replayed for the copies Timer E re-sends (§17.1.2.2), the rest absorbed.
#[tokio::test(start_paused = true)]
async fn a_non_invite_request_copied_at_wire_speed_draws_its_final_on_timer_es_schedule() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-info-copies", "info-copies");
    let info = inbound_request("INFO", branch, call_id, Some("b2bua-tag"));
    stack.inject(&info).await;
    elapse_ms(20).await;
    let [request] = handed_up(&stack.drain_events(), "INFO").try_into().expect("one INFO up");
    let ok = generate_response(&request, 200, "OK", &GenerateResponseOpts::default()).unwrap();
    stack.txn.send_response(ok, addr(PEER)).await.expect("the layer runs");
    elapse_ms(20).await;
    assert_eq!(answers(&stack.drain_peer(), 200, "INFO"), 1, "the TU's 200 leaves");

    burst(&stack, &info, COPIES).await;
    elapse_ms(20).await;
    let replayed = answers(&stack.drain_peer(), 200, "INFO");
    let resends = usize::try_from(Class::NonInviteClient.resends()).expect("fits");
    assert_eq!(replayed, resends, "the copies Timer E re-sends, then none inside T1");
}

/// A non-INVITE request re-sent every 2 s past 64·T1 of its first copy: the
/// copies inside that window are answered, and a copy past it, once Timer J
/// ended the transaction, re-opens none: the count outlives the transaction.
#[tokio::test(start_paused = true)]
async fn a_non_invite_request_re_sent_past_64_t1_is_neither_answered_nor_handed_up() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-info-past", "info-past");
    let info = inbound_request("INFO", branch, call_id, Some("b2bua-tag"));
    stack.inject(&info).await;
    elapse_ms(20).await;
    let [request] = handed_up(&stack.drain_events(), "INFO").try_into().expect("one INFO up");
    let ok = generate_response(&request, 200, "OK", &GenerateResponseOpts::default()).unwrap();
    stack.txn.send_response(ok, addr(PEER)).await.expect("the layer runs");
    elapse_ms(1_980).await;
    let _ = stack.drain_peer();

    let drawn = copies_every(&stack, &info, 2_000, WINDOW + 6_000, 200, "INFO").await;
    let (inside, past): (Vec<_>, Vec<_>) =
        drawn.into_iter().partition(|(at, _)| at + 2_000 < WINDOW);
    assert!(inside.iter().all(|(_, n)| *n == 1), "each copy T1 apart is answered: {inside:?}");
    let late: usize = past.iter().filter(|(at, _)| at + 2_000 > WINDOW).map(|(_, n)| n).sum();
    assert_eq!(late, 0, "no copy past 64·T1 is answered: {past:?}");
    assert!(handed_up(&stack.drain_events(), "INFO").is_empty(), "no copy re-opens a transaction");
}

/// A CANCEL of an answered INVITE re-sent at wire speed: the first and the
/// copies a CANCEL's Timer E re-sends draw a 200, the rest are absorbed.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_an_answered_invite_copied_at_wire_speed_draws_200_on_timer_es_schedule() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-cancel-copies", "cancel-copies");
    invite_answered(&mut stack, branch, call_id, 486).await;

    burst(&stack, &inbound_request("CANCEL", branch, call_id, None), 1 + COPIES).await;
    elapse_ms(20).await;
    let answered = answers(&stack.drain_peer(), 200, "CANCEL");
    let resends = usize::try_from(Class::CancelClient.resends()).expect("fits");
    assert_eq!(answered, 1 + resends, "the CANCEL and its Timer E re-sends");
    let absorbed = stack.txn.metrics().copies_past_schedule(CopyKind::Cancel);
    assert_eq!(absorbed, (1 + COPIES - answered) as u64, "each copy absorbed is counted");
}

/// Plays the TU of a CANCEL matching no INVITE (RFC 3261 §9.2): each one
/// handed up is answered 481. The count of those handed up.
async fn answer_481(stack: &mut Stack) -> usize {
    let cancels = handed_up(&stack.drain_events(), "CANCEL");
    for request in &cancels {
        let opts =
            GenerateResponseOpts { to_tag: Some("stray-tag".to_string()), ..Default::default() };
        let gone = generate_response(request, 481, "Call/Transaction Does Not Exist", &opts);
        stack.txn.send_response(gone.expect("a 481"), addr(PEER)).await.expect("the layer runs");
    }
    cancels.len()
}

/// A CANCEL matching no INVITE opens its own non-INVITE transaction: the TU
/// answers it once with the §9.2 481, and its copies at wire speed draw that
/// 481 for Timer E's re-sends, never reaching the TU.
#[tokio::test(start_paused = true)]
async fn a_stray_cancel_copied_at_wire_speed_is_answered_through_its_own_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let cancel = inbound_request("CANCEL", "z9hG4bK-stray-cancel", "stray-cancel", None);
    let mut handed = 0;
    for _ in 0..=COPIES {
        stack.inject(&cancel).await;
        elapse_ms(1).await;
        handed += answer_481(&mut stack).await;
    }
    elapse_ms(20).await;
    handed += answer_481(&mut stack).await;
    elapse_ms(20).await;
    let answered = answers(&stack.drain_peer(), 481, "CANCEL");
    let resends = usize::try_from(Class::CancelClient.resends()).expect("fits");
    assert_eq!(answered, 1 + resends, "its 481 and Timer E's re-sends");
    assert_eq!(handed, 1, "the TU answers the first CANCEL only");
}
