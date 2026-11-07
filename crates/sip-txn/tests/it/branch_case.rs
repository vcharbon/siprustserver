//! RFC 3261 §7.3.1: a parameter value compares ignoring case unless its
//! header says otherwise, and the Via `branch` (§20.42) says nothing else. A
//! copy of a request, its CANCEL and its non-2xx ACK whose branch a hop
//! re-cased belong to its server transaction (§17.2.3), and a response whose
//! branch was re-cased belongs to its client transaction (§17.1.3).

use crate::common;

use call::TxnKind;
use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::TIMER_H;
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;
const BRANCH: &str = "z9hG4bK-AbCd";
const RECASED: &str = "z9hG4bK-aBcD";
const TAG: &str = "uas-tag";

fn requests(events: &[TransactionEvent], method: &str) -> Vec<(SipRequest, std::net::SocketAddr)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some((r.clone(), *src)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn cancelled(events: &[TransactionEvent]) -> usize {
    events.iter().filter(|e| matches!(e, TransactionEvent::Cancelled { .. })).count()
}

/// The INVITE handed up once, its 100 sent.
async fn ringing(stack: &mut Stack, call_id: &str) -> (SipRequest, std::net::SocketAddr) {
    stack.inject(&inbound_request("INVITE", BRANCH, call_id, None)).await;
    elapse_ms(20).await;
    let mut up = requests(&stack.drain_events(), "INVITE");
    assert_eq!(up.len(), 1, "the INVITE surfaces once");
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1);
    up.remove(0)
}

#[tokio::test(start_paused = true)]
async fn a_recased_copy_of_an_invite_is_absorbed_by_its_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (req, src) = ringing(&mut stack, "copy@u").await;

    stack.inject(&inbound_request("INVITE", RECASED, "copy@u", None)).await;
    elapse_ms(20).await;
    let events = stack.drain_events();
    assert!(requests(&events, "INVITE").is_empty(), "the re-cased copy is absorbed: {events:?}");
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "and replays the 100");

    let opts = GenerateResponseOpts { to_tag: Some(TAG.to_string()), ..Default::default() };
    let busy = generate_response(&req, 486, "Busy Here", &opts).unwrap();
    stack.txn.send_response(busy, src).await.unwrap();
    stack.inject(&inbound_request("ACK", BRANCH, "copy@u", Some(TAG))).await;
    elapse_ms(TIMER_H + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "the transaction ended");
}

#[tokio::test(start_paused = true)]
async fn a_recased_cancel_cancels_its_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    ringing(&mut stack, "cancel@u").await;

    stack.inject(&inbound_request("CANCEL", RECASED, "cancel@u", None)).await;
    elapse_ms(20).await;
    assert_eq!(cancelled(&stack.drain_events()), 1, "the CANCEL names its INVITE");
    let wire = stack.drain_peer();
    assert_eq!((count_responses(&wire, 200), count_responses(&wire, 487)), (1, 1));
    stack.inject(&inbound_request("ACK", BRANCH, "cancel@u", Some(TAG))).await;
    elapse_ms(TIMER_H + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "the transaction ended");
}

#[tokio::test(start_paused = true)]
async fn a_recased_ack_ends_the_non_2xx_retransmissions() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (req, src) = ringing(&mut stack, "ack@u").await;
    let opts = GenerateResponseOpts { to_tag: Some(TAG.to_string()), ..Default::default() };
    let busy = generate_response(&req, 486, "Busy Here", &opts).unwrap();
    stack.txn.send_response(busy, src).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1);

    stack.inject(&inbound_request("ACK", RECASED, "ack@u", Some(TAG))).await;
    elapse_ms(20).await;
    assert!(requests(&stack.drain_events(), "ACK").is_empty(), "the ACK is absorbed");
    elapse_ms(4_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 0, "Timer G stopped on the ACK");
    elapse_ms(TIMER_H).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "the transaction ended");
}

#[tokio::test(start_paused = true)]
async fn a_response_with_a_recased_branch_matches_its_client_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let options = outbound_request("OPTIONS", BRANCH);
    stack.txn.send_request(options, addr(PEER), TxnKind::NonInvite).await.unwrap();
    elapse_ms(20).await;
    stack.drain_peer();

    stack.inject(&response_bytes(200, "OK", "OPTIONS", RECASED, "handle-shape-test", true)).await;
    elapse_ms(20).await;
    let matched: Vec<(u16, bool)> = stack
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => match &**message {
                SipMessage::Response(r) => Some((r.status(), *matched_client_txn)),
                SipMessage::Request(_) => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(matched, vec![(200, true)], "the re-cased 200 is the transaction's final");
    elapse_ms(1_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "OPTIONS"), 0, "no retransmission follows");
}
