//! `TransactionLayer::relinquish_call`: a call's transactions are deleted for
//! another node to serve — nothing sent, nothing owed, no self-release notice —
//! and a response for one of them arriving afterwards is handed up as a new
//! arrival, not ACKed by an orphan (RFC 3261 §13.2.2.4 is the serving node's).

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::{TransactionEvent, TxnSeed};

const CALL_REF: &str = "w0|relinquished@10.0.0.9|atag";
const CALL_ID: &str = "relinquished";

/// The call's two in-flight INVITEs, rebuilt as a takeover rebuilds them: the
/// callee's, answered provisionally, and the caller's, under its early dialog.
fn seeds(client: &str, server: &str) -> Vec<TxnSeed> {
    vec![
        TxnSeed::ClientInvite {
            invite: invite_with_cr_lg(CALL_REF, CALL_ID, client, "b-1"),
            dest: addr(PEER),
            provisional: true,
            cancel: None,
            ack_ahead: None,
        },
        TxnSeed::ServerInvite {
            request: request_of(&inbound_request("INVITE", server, CALL_ID, None)),
            to_tag: Some("early-tag".to_string()),
            leg_id: Some("a".to_string()),
        },
    ]
}

#[tokio::test(start_paused = true)]
async fn a_relinquished_call_sends_nothing_and_its_callees_answer_arrives_unmatched() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (client, server) = ("z9hG4bK-hb-c", "z9hG4bK-hb-s");
    assert_eq!(stack.txn.seed(CALL_REF, seeds(client, server)).await.unwrap(), 2);
    stack.txn.watch_self_release(CALL_REF).await.unwrap();

    assert_eq!(stack.txn.relinquish_call(CALL_REF).await.unwrap(), 2, "both go");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert_eq!(stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(), 0);
    assert_eq!(stack.txn.metrics().timer_queue_len(), 0, "their timers with them");
    elapse_ms(64_000).await;
    assert!(stack.drain_peer().is_empty(), "no ACK, CANCEL, final or retransmission");
    let quiesced = stack.drain_events();
    assert!(
        !quiesced.iter().any(|e| matches!(e, TransactionEvent::CallQuiesced { .. })),
        "the consumer released the call itself: no self-release notice",
    );

    stack.inject(&response_bytes(200, "OK", "INVITE", client, CALL_ID, true)).await;
    elapse_ms(60).await;
    let arrived: Vec<bool> = stack
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                matches!(message.as_ref(), SipMessage::Response(r) if r.status() == 200)
                    .then_some(*matched_client_txn)
            }
            _ => None,
        })
        .collect();
    assert_eq!(arrived, [false], "the 200 is handed up as a new arrival");
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "and no orphan ACKs it");
    assert_eq!(stack.txn.relinquish_call(CALL_REF).await.unwrap(), 0, "idempotent");
}

/// `TransactionLayer::relinquish_server`: one INVITE server transaction goes
/// with nothing sent — the release's answer of the call's unanswered INVITEs
/// no longer reaches it — while the call's other transactions stay.
#[tokio::test(start_paused = true)]
async fn a_relinquished_server_invite_draws_no_final_and_the_rest_stays() {
    let stack = Stack::build(5, 64, 64).await;
    let (client, server) = ("z9hG4bK-rs-c", "z9hG4bK-rs-s");
    assert_eq!(stack.txn.seed(CALL_REF, seeds(client, server)).await.unwrap(), 2);

    let key = sip_txn::ServerTxnKey::invite(server.to_string(), peer_sent_by());
    assert!(stack.txn.relinquish_server(key.clone()).await.unwrap(), "it was there");
    assert_eq!(stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(), 1, "the client stays");
    let answered = stack.txn.answer_unanswered_invites_of_call(CALL_REF, 481, "Gone").await;
    assert_eq!(answered.unwrap(), 0, "no INVITE left to answer");
    elapse_ms(100).await;
    let finals = stack.drain_peer().iter().filter(|m| matches!(m, SipMessage::Response(_))).count();
    assert_eq!(finals, 0, "no final");
    assert!(!stack.txn.relinquish_server(key).await.unwrap(), "idempotent");
}
