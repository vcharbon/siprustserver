//! `TransactionConfig::stateless_options`: an out-of-dialog OPTIONS opens no
//! server transaction, every copy reaches the consumer, and the consumer's
//! answer leaves raw (RFC 3261 §8.2.7). An in-dialog OPTIONS keeps its
//! transaction.

use std::sync::Arc;

use crate::common;
use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::SipMessage;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};

const TRANSIT: u64 = 5;

async fn stack() -> Stack {
    Stack::build_with_config(
        TRANSIT,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            stateless_options: true,
            ..Default::default()
        },
    )
    .await
}

fn messages(events: Vec<TransactionEvent>) -> Vec<sip_message::SipRequest> {
    events
        .into_iter()
        .filter_map(|ev| match ev {
            TransactionEvent::Message { message, .. } => match *message {
                SipMessage::Request(req) => Some(req),
                SipMessage::Response(_) => None,
            },
            _ => None,
        })
        .collect()
}

/// An out-of-dialog OPTIONS and its retransmission each reach the consumer,
/// each answer leaves on the wire, and no transaction is held for either.
#[tokio::test(start_paused = true)]
async fn an_out_of_dialog_options_opens_no_transaction_and_every_copy_is_delivered() {
    let mut stack = stack().await;
    let options = inbound_request("OPTIONS", "z9hG4bK-probe", "probe", None);

    for copy in 1..=2 {
        stack.inject(&options).await;
        elapse_ms(TRANSIT + 1).await;
        let delivered = messages(stack.drain_events());
        assert_eq!(delivered.len(), 1, "copy {copy} reaches the consumer");
        assert_eq!(stack.txn.metrics().active_transactions(), 0, "copy {copy} opens none");

        let answer = generate_response(
            &delivered[0],
            200,
            "OK",
            &GenerateResponseOpts { to_tag: Some("derived".into()), ..Default::default() },
        );
        stack.txn.send_response(answer, addr(PEER)).await.unwrap();
        elapse_ms(TRANSIT + 1).await;
        assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "copy {copy} is answered");
        assert_eq!(stack.txn.metrics().active_transactions(), 0, "the answer opens none");
    }
}

/// Another out-of-dialog request keeps its server transaction: only OPTIONS
/// is answered statelessly.
#[tokio::test(start_paused = true)]
async fn an_out_of_dialog_message_keeps_its_transaction() {
    let mut stack = stack().await;
    stack.inject(&inbound_request("MESSAGE", "z9hG4bK-message", "message", None)).await;
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(messages(stack.drain_events()).len(), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "it opens one");
}

/// An OPTIONS inside a dialog (To-tag) keeps its server transaction: its
/// retransmission is absorbed and answered from the cached response.
#[tokio::test(start_paused = true)]
async fn an_in_dialog_options_keeps_its_transaction() {
    let mut stack = stack().await;
    let options = inbound_request("OPTIONS", "z9hG4bK-in-dialog", "in-dialog", Some("b2bua-tag"));

    stack.inject(&options).await;
    elapse_ms(TRANSIT + 1).await;
    let delivered = messages(stack.drain_events());
    assert_eq!(delivered.len(), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "it opens one");

    let answer = generate_response(&delivered[0], 200, "OK", &GenerateResponseOpts::default());
    stack.txn.send_response(answer, addr(PEER)).await.unwrap();
    stack.inject(&options).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert!(messages(stack.drain_events()).is_empty(), "the copy is absorbed");
    assert_eq!(count_responses(&stack.drain_peer(), 200), 2, "answered, then replayed");
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "held for Timer J");
}
