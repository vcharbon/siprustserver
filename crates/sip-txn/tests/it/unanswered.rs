//! A request the consumer gives up on, at the transaction-layer seam: a
//! non-INVITE request discarded unanswered is forgotten so its retransmission
//! is admitted afresh, and a released call's open transactions are forgotten
//! or answered.

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

fn has_message_request(events: &[TransactionEvent], method: &str) -> bool {
    events.iter().any(|e| match e {
        TransactionEvent::Message { message, .. } => {
            matches!(message.as_ref(), SipMessage::Request(r) if r.method() == method)
        }
        _ => false,
    })
}

// ── forget_unanswered: the consumer discarded a request before answering ─────

/// A non-INVITE request the consumer discarded unanswered: forgetting its
/// `Trying` transaction lets the next copy in (RFC 3261 §17.1.2.2), where the
/// transaction would otherwise absorb it silently.
#[tokio::test(start_paused = true)]
async fn a_forgotten_trying_non_invite_admits_its_retransmission() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, bye) = ("z9hG4bK-forget", inbound_request("BYE", "z9hG4bK-forget", "f@u", None));
    stack.inject(&bye).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"));

    // Another request's identity on the same branch forgets nothing.
    let key = key_of(&bye);
    stack.txn.forget_unanswered(&key, "other@u", "caller-tag");
    stack.txn.forget_unanswered(&key, "f@u", "other-tag");
    elapse_ms(10).await;
    assert_eq!(active(&stack), 1, "a mismatching Call-ID or From-tag is not this transaction");
    assert_eq!(stack.txn.metrics().unanswered_forgotten(), 0);

    stack.txn.forget_unanswered(&key, "f@u", "caller-tag");
    elapse_ms(10).await;
    assert_eq!(active(&stack), 0);
    assert_eq!(stack.txn.metrics().unanswered_forgotten(), 1);

    stack.inject(&bye).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"), "the copy is admitted afresh");
    let resp = parse_response(&response_bytes(200, "OK", "BYE", branch, "f@u", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);
    assert_eq!(active(&stack), 0);
}

/// More forgets than the owner's command queue holds, posted without
/// yielding, are all applied: the discarded request's copy is admitted afresh,
/// never absorbed unanswered.
#[tokio::test(start_paused = true)]
async fn a_forget_past_the_command_queue_depth_still_admits_the_retransmission() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let bye = inbound_request("BYE", "z9hG4bK-forget-full", "full@u", None);
    stack.inject(&bye).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"));

    // Posted without yielding, so the owner drains none of them first.
    for n in 0..1100 {
        let other = inbound_request("OPTIONS", &format!("z9hG4bK-filler-{n}"), "x@u", None);
        stack.txn.forget_unanswered(&key_of(&other), "x@u", "caller-tag");
    }
    stack.txn.forget_unanswered(&key_of(&bye), "full@u", "caller-tag");
    elapse_ms(10).await;
    assert_eq!(active(&stack), 0, "the BYE's Trying transaction is forgotten");

    stack.inject(&bye).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"), "the copy is admitted afresh");
    let resp =
        parse_response(&response_bytes(200, "OK", "BYE", "z9hG4bK-forget-full", "full@u", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);
    assert_eq!(active(&stack), 0);
}

/// Only an unanswered non-INVITE transaction is forgotten: one that has sent
/// its final keeps replaying it (§17.2.2), and an INVITE transaction — whose
/// 100 Trying stopped the caller's retransmissions — is never touched.
#[tokio::test(start_paused = true)]
async fn forget_unanswered_keeps_answered_and_invite_transactions() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let answered = "z9hG4bK-answered";
    stack.inject(&inbound_request("OPTIONS", answered, "a@u", None)).await;
    stack.inject(&inbound_request("INVITE", "z9hG4bK-inv-kept", "i@u", None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let resp = parse_response(&response_bytes(200, "OK", "OPTIONS", answered, "a@u", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    let key = |method, branch, call_id| key_of(&inbound_request(method, branch, call_id, None));
    stack.txn.forget_unanswered(&key("OPTIONS", answered, "a@u"), "a@u", "caller-tag");
    stack.txn.forget_unanswered(&key("INVITE", "z9hG4bK-inv-kept", "i@u"), "i@u", "caller-tag");
    stack.txn.forget_unanswered(&key("OPTIONS", "z9hG4bK-unseen", "u@u"), "u@u", "caller-tag");
    elapse_ms(10).await;
    assert_eq!(active(&stack), 2, "neither transaction is forgotten");
    assert_eq!(stack.txn.metrics().unanswered_forgotten(), 0);

    stack.inject(&inbound_request("OPTIONS", answered, "a@u", None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the cached 200 is replayed");
    assert!(stack.drain_events().is_empty());
}

// ── forget_unanswered_of_call: the consumer released the call ────────────────

/// A request of a call, attributed to it by its Request-URI `callRef`.
fn call_request(method: &str, branch: &str, call_ref: &str, to_tag: Option<&str>) -> Vec<u8> {
    let to = match to_tag {
        Some(t) => format!("<sip:b2bua@127.0.0.1:5070>;tag={t}"),
        None => "<sip:b2bua@127.0.0.1:5070>".to_string(),
    };
    format!(
        "{method} sip:b2bua@127.0.0.1:5070;callRef={call_ref} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: {to}\r\n\
         Call-ID: {call_ref}@u\r\n\
         CSeq: 1 {method}\r\n\
         Contact: <sip:caller@10.0.0.1:5555>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// At its call's release, a non-INVITE server transaction of the call with no
/// final is forgotten, so its next copy is admitted afresh; one that has
/// answered keeps replaying its final, an INVITE transaction is kept, and
/// another call's transactions are untouched.
#[tokio::test(start_paused = true)]
async fn a_released_calls_unanswered_non_invite_transactions_are_forgotten() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let info = call_request("INFO", "z9hG4bK-info", "cr1", Some("callee-tag"));
    let answered = "z9hG4bK-opts";
    stack.inject(&info).await;
    stack.inject(&call_request("OPTIONS", answered, "cr1", Some("callee-tag"))).await;
    stack.inject(&call_request("INVITE", "z9hG4bK-reinv", "cr1", Some("callee-tag"))).await;
    stack.inject(&call_request("INFO", "z9hG4bK-other", "cr2", Some("callee-tag"))).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let resp = parse_response(&response_bytes(200, "OK", "OPTIONS", answered, "cr1@u", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    assert_eq!(active(&stack), 4);

    assert_eq!(stack.txn.forget_unanswered_of_call("cr1").await.unwrap(), 1);
    assert_eq!(active(&stack), 3, "only the unanswered INFO of cr1 is forgotten");
    assert_eq!(stack.txn.metrics().released_unanswered_forgotten(), 1);

    stack.inject(&info).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "INFO"), "the copy is admitted afresh");
    stack.inject(&call_request("OPTIONS", answered, "cr1", Some("callee-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the cached 200 is replayed");
    assert!(stack.drain_events().is_empty());
}

// ── answer_unanswered_invites_of_call: the consumer released an ended call ──

/// At its ended call's release, an INVITE server transaction of the call with
/// no final is answered through the transaction, which then absorbs the ACK
/// and stops its Timer G copies. An answered INVITE, a non-INVITE, another
/// call's INVITE, and an out-of-dialog INVITE naming the call are untouched.
#[tokio::test(start_paused = true)]
async fn a_released_calls_unanswered_invites_are_answered_through_their_transactions() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let open = "z9hG4bK-reinv-open";
    let answered = "z9hG4bK-reinv-done";
    stack.inject(&call_request("INVITE", open, "cr1", Some("callee-tag"))).await;
    // `response_bytes` answers under the To-tag `peer-tag`.
    stack.inject(&call_request("INVITE", answered, "cr1", Some("peer-tag"))).await;
    stack.inject(&call_request("INFO", "z9hG4bK-info", "cr1", Some("callee-tag"))).await;
    stack.inject(&call_request("INVITE", "z9hG4bK-other", "cr2", Some("callee-tag"))).await;
    // An out-of-dialog INVITE naming the call in its Request-URI (a Replaces
    // INVITE, RFC 3891) starts another call: it is not this call's to answer.
    stack.inject(&call_request("INVITE", "z9hG4bK-replaces", "cr1", None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let resp = parse_response(&response_bytes(200, "OK", "INVITE", answered, "cr1@u", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    let n = stack
        .txn
        .answer_unanswered_invites_of_call("cr1", 481, "Call/Transaction Does Not Exist")
        .await
        .unwrap();
    assert_eq!(n, 1, "only the open in-dialog INVITE of cr1 is answered");
    assert_eq!(stack.txn.metrics().released_unanswered_invites_answered(), 1);
    elapse_ms(60).await;
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 481), 1);
    assert_eq!(sent.len(), 1, "nothing else leaves");

    stack.inject(&call_request("ACK", open, "cr1", Some("callee-tag"))).await;
    elapse_ms(60).await;
    assert!(!has_message_request(&stack.drain_events(), "ACK"), "the ACK is absorbed");
    elapse_ms(8_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 481), 0, "the ACK stopped Timer G");
}
