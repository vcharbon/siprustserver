//! A CANCEL names the INVITE it cancels by that INVITE's CSeq number as well
//! as its branch (RFC 3261 §9.1). When no transaction holds the CANCEL's
//! branch, the INVITE it is matched against on its sent-by, Call-ID and
//! From-tag must carry its CSeq number; an INVITE of another number on the
//! same dialog is another transaction, and the CANCEL is handed up.

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

/// An INVITE answered 200 on one branch; a CANCEL on another branch with
/// another CSeq number — a later re-INVITE's — is not that INVITE's: no 200
/// from the layer, the CANCEL handed up.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_another_cseq_number_does_not_match_an_answered_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_id = "cxl-cseq-answered";
    stack.inject(&numbered("INVITE", "z9hG4bK-cseq-initial", call_id, None, 1)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let ok =
        parse_response(&response_bytes(200, "OK", "INVITE", "z9hG4bK-cseq-initial", call_id, true));
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

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

/// A CANCEL that kept the INVITE's CSeq number but not its branch is still
/// that INVITE's (a peer that did not keep the branch): 200 + 487.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_the_same_cseq_number_on_another_branch_cancels_the_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_id = "cxl-cseq-same";
    stack.inject(&numbered("INVITE", "z9hG4bK-cseq-invite", call_id, None, 3)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    stack.inject(&numbered("CANCEL", "z9hG4bK-cseq-other", call_id, None, 3)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "200 to the CANCEL: {out:?}");
    assert_eq!(count_responses(&out, 487), 1, "487 to the INVITE: {out:?}");
    assert!(cancelled(&stack.drain_events()));
}
