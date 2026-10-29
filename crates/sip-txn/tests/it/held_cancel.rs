//! The answers to a CANCEL of an active INVITE server transaction under a
//! consumer that releases them (`cancel_bound_ms`, RFC 3261 §9.2): held until
//! released or until their bound passes, a copy of the CANCEL or the INVITE
//! absorbed meanwhile, the 487 repeated on Timer G once it left.

use crate::common::{self, *};
use sip_txn::{TransactionConfig, TransactionEvent};

const TRANSIT: u64 = 5;
const BOUND: u64 = 200;
const CALL_ID: &str = "held-cxl";

async fn holding() -> Stack {
    let config = TransactionConfig { cancel_bound_ms: BOUND, ..common::config() };
    Stack::build_with_config(TRANSIT, 64, config).await
}

/// An INVITE admitted, its 100 drained, then its CANCEL.
async fn cancelled(stack: &mut Stack, branch: &str) {
    stack.inject(&inbound_request("INVITE", branch, CALL_ID, None)).await;
    elapse_ms(20).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();
    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, None)).await;
    elapse_ms(20).await;
}

fn answers(stack: &Stack) -> (usize, usize) {
    let out = stack.drain_peer();
    (count_responses(&out, 200), count_responses(&out, 487))
}

#[tokio::test(start_paused = true)]
async fn the_answers_to_a_cancel_leave_on_release_and_copies_meanwhile_draw_none() {
    let mut stack = holding().await;
    let branch = "z9hG4bK-held-cxl-release";
    cancelled(&mut stack, branch).await;
    assert_eq!(answers(&stack), (0, 0), "the 200 and the 487 are held");
    let events = stack.drain_events();
    let Some(TransactionEvent::Cancelled { invite_cseq, from_tag, .. }) =
        events.iter().find(|e| matches!(e, TransactionEvent::Cancelled { .. }))
    else {
        panic!("the Cancelled event goes up at once: {events:?}");
    };
    let (cseq, from_tag) = (invite_cseq.expect("the INVITE's CSeq"), from_tag.clone());

    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, None)).await;
    stack.inject(&inbound_request("INVITE", branch, CALL_ID, None)).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (0, 0), "copies of the CANCEL and the INVITE draw nothing");

    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq).await.unwrap(), "released");
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 1), "the 200 and the 487 leave");
    assert!(!stack.txn.release_cancel(CALL_ID, &from_tag, cseq).await.unwrap());
    elapse_ms(600).await;
    assert_eq!(answers(&stack).1, 1, "the 487 repeats on Timer G until the ACK");
}

#[tokio::test(start_paused = true)]
async fn the_answers_nobody_releases_leave_at_their_bound() {
    let mut stack = holding().await;
    let branch = "z9hG4bK-held-cxl-bound";
    cancelled(&mut stack, branch).await;
    elapse_ms(BOUND - 60).await;
    assert_eq!(answers(&stack), (0, 0), "held inside the bound");
    elapse_ms(60).await;
    assert_eq!(answers(&stack), (1, 1), "sent at the bound");
}
