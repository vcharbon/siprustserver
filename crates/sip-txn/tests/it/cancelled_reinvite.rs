//! A CANCEL of an in-dialog INVITE under a consumer that ends it
//! (`consumer_ends_cancelled_reinvite`, RFC 3261 §9.2, §16.10): the CANCEL's
//! 200 and the 487 are held at any bound, the release keeps the INVITE open or
//! ends it however late it comes, the bound sends the 200 alone, a release
//! never coming ends it 64·T1 past the CANCEL, a final of its own drops the
//! 487, and a copy of the CANCEL draws its 200 again and reaches the consumer
//! never. The initial INVITE keeps the immediate 487.
//! The client side: a CANCEL on an INVITE in Proceeding brings its give-up
//! within 64·T1 (§9.1).

use call::TxnKind;
use sip_txn::timers::{TIMER_B, TRYING_BOUND};
use sip_txn::{CancelledInvite, TransactionConfig, TransactionEvent};

use crate::common::{self, *};

const TRANSIT: u64 = 5;
const CALL_ID: &str = "reinvite-cxl";
const OUR_TAG: &str = "b2bua-tag";

async fn ending_stack() -> Stack {
    let config = TransactionConfig { consumer_ends_cancelled_reinvite: true, ..common::config() };
    Stack::build_with_config(TRANSIT, 64, config).await
}

/// A re-INVITE admitted, its 100 drained, then its CANCEL: the CSeq and
/// From-tag the `Cancelled` event names, and that it named an in-dialog INVITE.
async fn cancelled(stack: &mut Stack, branch: &str, to_tag: Option<&str>) -> (u32, String, bool) {
    stack.inject(&inbound_request("INVITE", branch, CALL_ID, to_tag)).await;
    elapse_ms(20).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();
    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, to_tag)).await;
    elapse_ms(20).await;
    let events = stack.drain_events();
    let Some(TransactionEvent::Cancelled { invite_cseq, from_tag, in_dialog, .. }) =
        events.iter().find(|e| matches!(e, TransactionEvent::Cancelled { .. }))
    else {
        panic!("the Cancelled event goes up: {events:?}");
    };
    (invite_cseq.expect("the INVITE's CSeq"), from_tag.clone(), *in_dialog)
}

fn answers(stack: &Stack) -> (usize, usize) {
    let out = stack.drain_peer();
    (count_responses(&out, 200), count_responses(&out, 487))
}

#[tokio::test(start_paused = true)]
async fn kept_open_the_reinvite_takes_the_consumers_final_and_no_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-keep";
    let (cseq, from_tag, in_dialog) = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    assert!(in_dialog, "the CANCEL names a re-INVITE");
    assert_eq!(answers(&stack), (0, 0), "held even with no cancel bound");

    let keep = CancelledInvite::Keep;
    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq, keep).await.unwrap());
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 0), "the CANCEL's 200 leaves, no 487");

    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, Some(OUR_TAG))).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 0), "a copy of the CANCEL draws its 200 again");
    let events = stack.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "a copy reaches the consumer never: {events:?}"
    );

    elapse_ms(TRYING_BOUND + 600).await;
    let out = stack.drain_peer();
    assert!(out.is_empty(), "past the bound, nothing answers the re-INVITE: {out:?}");
    let ok = parse_response(
        format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
             From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
             To: <sip:b2bua@127.0.0.1:5070>;tag={OUR_TAG}\r\n\
             Call-ID: {CALL_ID}\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:b2bua@127.0.0.1:5070>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 0), "the consumer's final answers the re-INVITE");
}

#[tokio::test(start_paused = true)]
async fn ended_on_release_the_reinvite_takes_the_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-end";
    let (cseq, from_tag, _) = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    let end = CancelledInvite::End;
    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq, end).await.unwrap());
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 1), "the 200 and the 487 leave");
    elapse_ms(600).await;
    assert_eq!(answers(&stack).1, 1, "the 487 repeats on Timer G until the ACK");
}

/// The bound sends the CANCEL's 200 alone; the 487 waits for a release that
/// keeps the re-INVITE open however late it comes, and a copy of the CANCEL
/// draws the 200 again meanwhile.
#[tokio::test(start_paused = true)]
async fn past_the_bound_a_late_keep_still_leaves_no_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-late-keep";
    let (cseq, from_tag, _) = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    elapse_ms(TRYING_BOUND - 60).await;
    assert_eq!(answers(&stack), (0, 0), "held inside the bound");
    elapse_ms(60).await;
    assert_eq!(answers(&stack), (1, 0), "the bound sends the CANCEL's 200 alone");
    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, Some(OUR_TAG))).await;
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (1, 0), "a copy of the CANCEL draws its 200 again");

    elapse_ms(2_000).await;
    let keep = CancelledInvite::Keep;
    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq, keep).await.unwrap());
    elapse_ms(TIMER_B).await;
    assert_eq!(answers(&stack), (0, 0), "kept open, the re-INVITE takes no 487");
}

/// Past the bound, a release that ends the re-INVITE sends its 487.
#[tokio::test(start_paused = true)]
async fn past_the_bound_a_late_end_sends_the_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-late-end";
    let (cseq, from_tag, _) = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    elapse_ms(TRYING_BOUND + 1_000).await;
    assert_eq!(answers(&stack), (1, 0), "the bound sent the CANCEL's 200 alone");
    let end = CancelledInvite::End;
    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq, end).await.unwrap());
    elapse_ms(20).await;
    assert_eq!(answers(&stack), (0, 1), "the late release ends the re-INVITE");
}

/// No release ever comes: the re-INVITE ends with the 487 64·T1 past the
/// CANCEL (RFC 3261 §9.1).
#[tokio::test(start_paused = true)]
async fn unreleased_the_reinvite_ends_with_the_487_64_t1_past_the_cancel() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-unreleased";
    let _ = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    elapse_ms(TRYING_BOUND).await;
    assert_eq!(answers(&stack), (1, 0), "the bound sends the CANCEL's 200 alone");
    elapse_ms(TIMER_B - TRYING_BOUND - 200).await;
    assert_eq!(answers(&stack), (0, 0), "inside 64·T1 of the CANCEL, still open");
    elapse_ms(400).await;
    assert_eq!(answers(&stack), (0, 1), "64·T1 past the CANCEL, the 487 ends it");
}

/// The re-INVITE takes a final of its own while its 487 is held: that 487 is
/// no longer owed, so neither a release nor the transaction's end sends a
/// second final (RFC 3261 §17.2.1).
#[tokio::test(start_paused = true)]
async fn a_final_of_its_own_drops_the_held_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-reinvite-cxl-own-final";
    let (cseq, from_tag, _) = cancelled(&mut stack, branch, Some(OUR_TAG)).await;
    let busy = parse_response(
        format!(
            "SIP/2.0 500 Server Internal Error\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
             From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
             To: <sip:b2bua@127.0.0.1:5070>;tag={OUR_TAG}\r\n\
             Call-ID: {CALL_ID}\r\n\
             CSeq: 1 INVITE\r\n\
             Retry-After: 5\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    elapse_ms(20).await;
    let end = CancelledInvite::End;
    assert!(stack.txn.release_cancel(CALL_ID, &from_tag, cseq, end).await.unwrap());
    elapse_ms(TIMER_B + 10_000).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 487), 0, "no second final: {out:?}");
    assert_eq!(count_responses(&out, 200), 1, "the CANCEL's 200 still leaves");
}

#[tokio::test(start_paused = true)]
async fn the_initial_invites_cancel_keeps_the_immediate_487() {
    let mut stack = ending_stack().await;
    let branch = "z9hG4bK-initial-cxl";
    let (_, _, in_dialog) = cancelled(&mut stack, branch, None).await;
    assert!(!in_dialog);
    assert_eq!(answers(&stack), (1, 1), "answered 200 and 487 as the CANCEL arrives");
}

/// A CANCEL leaving on an INVITE client transaction in Proceeding brings its
/// give-up from the INVITE bound (minutes) to 64·T1 past the CANCEL.
#[tokio::test(start_paused = true)]
async fn a_cancelled_invite_in_proceeding_gives_up_64_t1_past_the_cancel() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-cxl-give-up";
    let invite = outbound_request("INVITE", branch);
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(20).await;
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(5_000).await;
    let cancel = outbound_request("CANCEL", branch);
    stack.txn.send_request(cancel, addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    let _ = stack.drain_events();

    elapse_ms(TIMER_B - 200).await;
    let timed_out = |events: &[TransactionEvent]| {
        events.iter().any(|e| matches!(e, TransactionEvent::Timeout { .. }))
    };
    assert!(!timed_out(&stack.drain_events()), "inside 64·T1 of the CANCEL");
    elapse_ms(400).await;
    assert!(timed_out(&stack.drain_events()), "64·T1 past the CANCEL, the INVITE gives up");
}
