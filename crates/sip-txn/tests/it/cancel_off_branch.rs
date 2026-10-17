//! A CANCEL is matched to its INVITE by the INVITE's top-Via branch and
//! sent-by alone (RFC 3261 §9.1, §9.2, §17.2.3). A CANCEL on any other branch
//! matches no INVITE server transaction here, whatever its Call-ID, From-tag
//! and CSeq number: the layer neither answers it nor touches an INVITE, and
//! hands it up to the TU.

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;

/// `inbound_request` with CSeq number `cseq`.
fn numbered(method: &str, branch: &str, call_id: &str, to_tag: Option<&str>, cseq: u32) -> Vec<u8> {
    String::from_utf8(inbound_request(method, branch, call_id, to_tag))
        .expect("utf-8")
        .replace(&format!("CSeq: 1 {method}"), &format!("CSeq: {cseq} {method}"))
        .into_bytes()
}

/// Whether a CANCEL was handed up in a drained batch.
fn cancel_handed_up(events: &[TransactionEvent]) -> bool {
    events.iter().any(|e| {
        matches!(e, TransactionEvent::Message { message, .. }
            if matches!(message.as_ref(), SipMessage::Request(r) if r.method() == "CANCEL"))
    })
}

fn cancelled(events: &[TransactionEvent]) -> bool {
    events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. }))
}

/// Stack with an INVITE (CSeq `cseq`) on `branch` answered 200.
async fn answered_invite(call_id: &str, branch: &str, cseq: u32) -> Stack {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&numbered("INVITE", branch, call_id, None, cseq)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let ok = String::from_utf8(response_bytes(200, "OK", "INVITE", branch, call_id, true))
        .expect("utf-8")
        .replace("CSeq: 1 INVITE", &format!("CSeq: {cseq} INVITE"));
    let ok = parse_response(ok.as_bytes());
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    stack
}

/// An INVITE answered 200 on one branch; a CANCEL on another branch with
/// another CSeq number — a later re-INVITE's — is not that INVITE's: no 200
/// from the layer, the CANCEL handed up.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_another_cseq_number_does_not_match_an_answered_invite() {
    let call_id = "cxl-cseq-answered";
    let mut stack = answered_invite(call_id, "z9hG4bK-cseq-initial", 1).await;

    stack.inject(&numbered("CANCEL", "z9hG4bK-cseq-reinvite", call_id, Some("peer-tag"), 7)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 0, "the layer does not answer it: {out:?}");
    assert!(cancel_handed_up(&stack.drain_events()), "the CANCEL is the TU's");
}

/// A re-INVITE open on one branch; a CANCEL on another branch naming an
/// earlier CSeq number — the CANCEL of a round already ended, retransmitted —
/// cancels nothing: no 487, no `Cancelled`, the open round untouched.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_an_earlier_round_does_not_cancel_the_open_one() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_id = "cxl-cseq-open";
    stack.inject(&numbered("INVITE", "z9hG4bK-cseq-round6", call_id, Some("peer-tag"), 6)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    stack.inject(&numbered("CANCEL", "z9hG4bK-cseq-round5", call_id, Some("peer-tag"), 5)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 487), 0, "the open round is not cancelled: {out:?}");
    assert_eq!(count_responses(&out, 200), 0, "the layer does not answer it: {out:?}");
    let events = stack.drain_events();
    assert!(!cancelled(&events));
    assert!(cancel_handed_up(&events), "the CANCEL is the TU's");
}

/// An INVITE open on one branch; a CANCEL that kept its Call-ID, From-tag and
/// CSeq number but not its branch is not its CANCEL: no 200, no 487, no
/// `Cancelled`, the CANCEL handed up and the INVITE left open.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_the_same_cseq_number_on_another_branch_does_not_cancel_the_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_id = "cxl-cseq-same";
    stack.inject(&numbered("INVITE", "z9hG4bK-cseq-invite", call_id, None, 3)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    stack.inject(&numbered("CANCEL", "z9hG4bK-cseq-other", call_id, None, 3)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 487), 0, "the INVITE is not cancelled: {out:?}");
    assert_eq!(count_responses(&out, 200), 0, "the layer does not answer it: {out:?}");
    let events = stack.drain_events();
    assert!(!cancelled(&events));
    assert!(cancel_handed_up(&events), "the CANCEL is the TU's");
}

/// An INVITE answered 200 on one branch; a CANCEL that kept its Call-ID,
/// From-tag and CSeq number but not its branch is not its CANCEL: no 200
/// from the layer, the CANCEL handed up.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_the_same_cseq_number_on_another_branch_does_not_match_an_answered_invite() {
    let call_id = "cxl-cseq-same-answered";
    let mut stack = answered_invite(call_id, "z9hG4bK-cseq-answered", 4).await;

    stack.inject(&numbered("CANCEL", "z9hG4bK-cseq-late", call_id, None, 4)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 0, "the layer does not answer it: {out:?}");
    assert!(cancel_handed_up(&stack.drain_events()), "the CANCEL is the TU's");
}
