//! `TransactionLayer::relinquish_unanswered_server`: a server transaction
//! that has sent no final and holds no answers to a CANCEL goes with nothing
//! sent; one that sent its final, or took a CANCEL, keeps serving its request
//! (RFC 3261 §17.2.1, §9.2).

use crate::common::{self, *};
use sip_txn::TransactionConfig;

const CALL_ID: &str = "unanswered-relinquish";

fn finals(stack: &Stack) -> usize {
    let out = stack.drain_peer();
    out.iter()
        .filter(|m| matches!(m, sip_message::SipMessage::Response(r) if r.status() >= 200))
        .count()
}

#[tokio::test(start_paused = true)]
async fn an_invite_with_no_final_goes_with_nothing_sent() {
    let stack = Stack::build(5, 64, 64).await;
    let invite = inbound_request("INVITE", "z9hG4bK-ru-proceeding", CALL_ID, None);
    stack.inject(&invite).await;
    elapse_ms(20).await;

    assert!(stack.txn.relinquish_unanswered_server(key_of(&invite)).await.unwrap(), "it went");
    elapse_ms(40_000).await;
    assert_eq!(finals(&stack), 0, "no final");
    assert!(!stack.txn.relinquish_unanswered_server(key_of(&invite)).await.unwrap(), "idempotent");
}

#[tokio::test(start_paused = true)]
async fn a_non_invite_still_trying_goes() {
    let stack = Stack::build(5, 64, 64).await;
    let info = inbound_request("INFO", "z9hG4bK-ru-trying", CALL_ID, Some("callee-tag"));
    stack.inject(&info).await;
    elapse_ms(20).await;

    assert!(stack.txn.relinquish_unanswered_server(key_of(&info)).await.unwrap(), "it went");
    assert_eq!(finals(&stack), 0, "no final");
}

#[tokio::test(start_paused = true)]
async fn an_invite_that_sent_its_final_keeps_serving_it() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-ru-completed";
    let invite = inbound_request("INVITE", branch, CALL_ID, None);
    stack.inject(&invite).await;
    elapse_ms(20).await;
    let busy = parse_response(&response_bytes(486, "Busy Here", "INVITE", branch, CALL_ID, true));
    stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(finals(&stack), 1);

    assert!(!stack.txn.relinquish_unanswered_server(key_of(&invite)).await.unwrap(), "refused");
    stack.inject(&invite).await;
    elapse_ms(20).await;
    assert!(finals(&stack) >= 1, "its retransmission meets the final again");
}

#[tokio::test(start_paused = true)]
async fn an_invite_holding_a_cancels_answers_keeps_them() {
    let config = TransactionConfig { cancel_bound_ms: 200, ..common::config() };
    let stack = Stack::build_with_config(5, 64, config).await;
    let branch = "z9hG4bK-ru-cancelled";
    let invite = inbound_request("INVITE", branch, CALL_ID, None);
    stack.inject(&invite).await;
    elapse_ms(20).await;
    let _ = stack.drain_peer();
    stack.inject(&inbound_request("CANCEL", branch, CALL_ID, None)).await;
    elapse_ms(20).await;

    assert!(!stack.txn.relinquish_unanswered_server(key_of(&invite)).await.unwrap(), "refused");
    elapse_ms(200).await;
    let out = stack.drain_peer();
    assert_eq!((count_responses(&out, 200), count_responses(&out, 487)), (1, 1), "both leave");
}

#[tokio::test(start_paused = true)]
async fn an_absent_transaction_is_not_there() {
    let stack = Stack::build(5, 64, 64).await;
    let invite = inbound_request("INVITE", "z9hG4bK-ru-absent", CALL_ID, None);
    assert!(!stack.txn.relinquish_unanswered_server(key_of(&invite)).await.unwrap());
}
