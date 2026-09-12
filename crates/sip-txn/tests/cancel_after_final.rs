//! RFC 3261 §9.2 for a CANCEL that lands after the INVITE's final left: the
//! server transaction is still held — Accepted for Timer L after a 2xx (RFC
//! 6026 §7.1), Completed for Timer H after a non-2xx (§17.2.1) — so the CANCEL
//! matches it, has no effect, and is answered 200 under the To-tag the final
//! carried. No 481 is left to the TU, no 487 and no second final go out.

mod common;
use common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

/// INVITE in, the TU's final `status` out; the wire and the events drained.
async fn invite_answered(stack: &mut Stack, branch: &str, call_id: &str, status: u16) {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
    let resp = parse_response(&response_bytes(status, "Final", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
}

/// The one response the CANCEL drew, or a failure naming what went out instead.
fn the_cancel_answer(out: &[SipMessage]) -> &sip_message::SipResponse {
    let answers: Vec<_> = out
        .iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.cseq().method() == "CANCEL" => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 1, "exactly one answer to the CANCEL: {out:?}");
    answers[0]
}

#[tokio::test(start_paused = true)]
async fn cancel_after_a_2xx_final_is_answered_200_under_the_finals_tag() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-cxl-after-2xx", "cxl-after-2xx");
    invite_answered(&mut stack, branch, call_id, 200).await;

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    let answer = the_cancel_answer(&out);
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.to().tag(), Some("peer-tag"), "the To-tag is the 2xx's (RFC 3261 §9.2)");
    assert_eq!(count_responses(&out, 487), 0, "no 487 on an answered INVITE: {out:?}");
    assert_eq!(count_requests(&out, "INVITE"), 0);
    let events = stack.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "a CANCEL after the final tears nothing down"
    );
    assert!(
        !events.iter().any(|e| matches!(e, TransactionEvent::Message { message, .. }
            if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == "CANCEL"))),
        "the layer answered the CANCEL; the TU never sees it"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_after_a_non_2xx_final_is_answered_200_and_the_final_stands() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-cxl-after-486", "cxl-after-486");
    invite_answered(&mut stack, branch, call_id, 486).await;

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let answer = the_cancel_answer(&out);
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.to().tag(), Some("peer-tag"));
    assert_eq!(count_responses(&out, 487), 0, "the 486 is the transaction's one final: {out:?}");
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "no Cancelled for a CANCEL after the final"
    );
}
