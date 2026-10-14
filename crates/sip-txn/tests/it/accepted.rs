//! RFC 6026 §7.1 / §8.7: an INVITE server transaction that sent a 2xx is
//! Accepted until Timer L. It absorbs copies of its INVITE, drawing the 2xx
//! the TU sent and never a 100 or a second transaction, and passes every ACK
//! up; Timer L, measured from the 2xx, takes it out of the map.

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::timers::TIMER_L;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// The requests of `method` handed up in a drained batch.
fn handed_up(events: &[TransactionEvent], method: &str) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == method))
        })
        .count()
}

/// `inbound_request` addressed to the call `call_ref` names in its
/// Request-URI, as an in-dialog request routed to this node carries it.
fn in_dialog_request(method: &str, branch: &str, call_id: &str, call_ref: &str) -> Vec<u8> {
    String::from_utf8(inbound_request(method, branch, call_id, Some("peer-tag")))
        .expect("utf-8")
        .replacen(
            "sip:b2bua@127.0.0.1:5070 SIP/2.0",
            &format!("sip:b2bua@127.0.0.1:5070;callRef={call_ref} SIP/2.0"),
            1,
        )
        .into_bytes()
}

/// Admit `invite` and answer it 200 through its transaction; the 100 and
/// the 200 are drained.
async fn answer_200(stack: &mut Stack, invite: &[u8]) {
    stack.inject(invite).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the auto-100");
    assert_eq!(handed_up(&stack.drain_events(), "INVITE"), 1, "the INVITE is handed up once");
    let req = request_of(invite);
    let branch = req.top_via().branch().expect("a branch").to_string();
    let ok =
        parse_response(&response_bytes(200, "OK", "INVITE", &branch, req.call_id().as_str(), true));
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the 2xx leaves once");
}

/// An INVITE answered 2xx, then ACKed on its own branch: the ACK passes up and
/// the transaction stays Accepted, so a delayed copy of the INVITE is absorbed
/// with the 2xx and opens nothing; a repeated ACK passes up again; Timer L
/// purges the transaction.
#[tokio::test(start_paused = true)]
async fn an_answered_invite_absorbs_a_late_copy_until_timer_l() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-accepted";
    let call_id = "accepted-call";
    let invite = inbound_request("INVITE", branch, call_id, None);
    answer_200(&mut stack, &invite).await;

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(handed_up(&stack.drain_events(), "ACK"), 1, "the ACK passes up");
    assert_eq!(active(&stack), 1, "Accepted holds the transaction past the ACK");

    stack.inject(&invite).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 100), 0, "no 100 for a copy of an answered INVITE");
    assert_eq!(count_responses(&out, 200), 1, "the copy draws the 2xx the TU sent");
    assert_eq!(handed_up(&stack.drain_events(), "INVITE"), 0, "the copy is not handed up");
    assert_eq!(active(&stack), 1, "no transaction opens beside the Accepted one");

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(handed_up(&stack.drain_events(), "ACK"), 1, "a repeated ACK passes up too");
    assert!(stack.drain_peer().is_empty(), "nothing leaves on an ACK");

    // 240 ms have passed since the 2xx.
    elapse_ms(TIMER_L - 500).await;
    assert_eq!(active(&stack), 1, "held inside Timer L");
    elapse_ms(500).await;
    assert_eq!(active(&stack), 0, "Timer L purges the Accepted transaction");
}

/// The ACK of an Accepted transaction's 2xx takes it off its call's books,
/// as the ACK of a non-2xx does in Confirmed: its obligation to the call is
/// met, and only copies of the INVITE remain for it to absorb.
#[tokio::test(start_paused = true)]
async fn the_ack_takes_an_accepted_transaction_off_its_calls_books() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_ref = "w0|accepted-books|atag";
    let (branch, call_id) = ("z9hG4bK-accepted-books", "accepted-books");
    answer_200(&mut stack, &in_dialog_request("INVITE", branch, call_id, call_ref)).await;
    assert_eq!(
        stack.txn.active_txn_count_for_call(call_ref).await.unwrap(),
        1,
        "the answered INVITE counts for its call until its ACK"
    );

    stack.inject(&in_dialog_request("ACK", branch, call_id, call_ref)).await;
    elapse_ms(60).await;
    assert_eq!(handed_up(&stack.drain_events(), "ACK"), 1);
    assert_eq!(active(&stack), 1, "still Accepted");
    assert_eq!(
        stack.txn.active_txn_count_for_call(call_ref).await.unwrap(),
        0,
        "an ACKed Accepted transaction no longer counts for its call"
    );
}
