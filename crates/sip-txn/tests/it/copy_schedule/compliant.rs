//! The other side of ADR-0063 §1: a compliant client re-sending on its own
//! ladder, every answer lost on the way, is answered on every copy, and a
//! later transaction of a long call counts its copies afresh.

use std::time::Duration;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::SipMessage;
use sip_retransmit::timers::{T1, T2};
use sip_txn::TransactionEvent;
use tokio::time::Instant;

use super::{answers, burst, COPIES};
use crate::common::*;

const TRANSIT: u64 = 5;

/// The times after the original a compliant client re-sends it, in ms: Timer
/// A's for an INVITE (RFC 3261 §17.1.1.2), Timer E's for a non-INVITE request
/// (§17.1.2.2), both up to 64·T1.
fn ladder(capped: bool) -> Vec<u64> {
    let (mut at, mut gap, mut rungs) = (0, T1, Vec::new());
    while at + gap < 64 * T1 {
        at += gap;
        rungs.push(at);
        gap = if capped { (gap * 2).min(T2) } else { gap * 2 };
    }
    rungs
}

/// Sends `raw` at each of `offsets` ms after `start`, returning the
/// responses of `status` to a `method` request each copy drew.
async fn copies_at(
    stack: &Stack,
    raw: &[u8],
    start: Instant,
    offsets: &[u64],
    status: u16,
    method: &str,
) -> Vec<usize> {
    let mut drawn = Vec::new();
    for offset in offsets {
        tokio::time::sleep_until(start + Duration::from_millis(*offset)).await;
        stack.inject(raw).await;
        elapse_ms(20).await;
        drawn.push(answers(&stack.drain_peer(), status, method));
    }
    drawn
}

/// The INFO handed up in a drained batch, answered 200 by the TU; the wire
/// drained past that 200.
async fn answer_info(stack: &mut Stack) {
    let events = stack.drain_events();
    let request = events
        .iter()
        .find_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == "INFO" => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the INFO goes up");
    let ok = generate_response(&request, 200, "OK", &GenerateResponseOpts::default()).unwrap();
    stack.txn.send_response(ok, addr(PEER)).await.expect("the layer runs");
    elapse_ms(20).await;
    let _ = stack.drain_peer();
}

/// A ringing INVITE whose caller loses every 180 and re-sends on Timer A:
/// every copy draws the 180.
#[tokio::test(start_paused = true)]
async fn a_caller_losing_every_180_is_answered_on_each_timer_a_copy() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-lossy-ringing", "lossy-ringing");
    let invite = inbound_request("INVITE", branch, call_id, None);
    let start = Instant::now();
    stack.inject(&invite).await;
    elapse_ms(20).await;
    let _ = stack.drain_events();
    let ringing = parse_response(&response_bytes(180, "Ringing", "INVITE", branch, call_id, true));
    stack.txn.send_response(ringing, addr(PEER)).await.expect("the layer runs");
    elapse_ms(20).await;
    let _ = stack.drain_peer();

    let rungs = ladder(false);
    let drawn = copies_at(&stack, &invite, start, &rungs, 180, "INVITE").await;
    assert_eq!(drawn, vec![1; rungs.len()], "every copy at {rungs:?} draws the 180");
}

/// A non-INVITE request whose sender loses every 200 and re-sends on Timer
/// E: every copy draws the 200.
#[tokio::test(start_paused = true)]
async fn a_sender_losing_every_200_is_answered_on_each_timer_e_copy() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let info = inbound_request("INFO", "z9hG4bK-lossy-info", "lossy-info", Some("b2bua-tag"));
    let start = Instant::now();
    stack.inject(&info).await;
    elapse_ms(20).await;
    answer_info(&mut stack).await;

    let rungs = ladder(true);
    let drawn = copies_at(&stack, &info, start, &rungs, 200, "INFO").await;
    assert_eq!(drawn, vec![1; rungs.len()], "every copy at {rungs:?} draws the 200");
}

/// A long call's later INFO, on a new branch of a Call-ID whose earlier INFO
/// was copied past its schedule: its answer lost, each Timer E copy draws it.
#[tokio::test(start_paused = true)]
async fn a_later_request_of_a_long_call_counts_its_copies_afresh() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_id = "long-call";
    let first = inbound_request("INFO", "z9hG4bK-long-1", call_id, Some("b2bua-tag"));
    stack.inject(&first).await;
    elapse_ms(20).await;
    answer_info(&mut stack).await;
    burst(&stack, &first, COPIES).await;
    elapse_ms(120_000).await;
    let _ = stack.drain_peer();

    let later = inbound_request("INFO", "z9hG4bK-long-2", call_id, Some("b2bua-tag"));
    let start = Instant::now();
    stack.inject(&later).await;
    elapse_ms(20).await;
    answer_info(&mut stack).await;
    let rungs = ladder(true);
    let drawn = copies_at(&stack, &later, start, &rungs, 200, "INFO").await;
    assert_eq!(drawn, vec![1; rungs.len()], "every copy at {rungs:?} draws the 200");
}
