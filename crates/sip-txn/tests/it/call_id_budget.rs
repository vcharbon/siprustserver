//! `TransactionConfig::call_id_budget` (ADR-0063 §3, §4): every datagram the
//! layer receives and sends under a Call-ID counts against its budget, raw
//! sends included; the one past the budget mutes the Call-ID, which then
//! sends nothing and drops what arrives, and the consumer is told once, with
//! the callers and calls its transactions name. Out-of-dialog OPTIONS on the
//! keepalive lane spend no budget.

use std::sync::Arc;

use crate::common;
use common::*;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_txn::{CallIdBudget, CallIdBudgetTrip, IdGen, TransactionConfig, TransactionEvent};

const TRANSIT: u64 = 5;

async fn stack(messages: u64) -> Stack {
    Stack::build_with_config(
        TRANSIT,
        1024,
        TransactionConfig {
            id_gen: Arc::new(IdGen::seeded(0xb0d6)),
            stateless_options: true,
            call_id_budget: Some(CallIdBudget { messages, entries: CallIdBudget::ENTRIES }),
            ..common::config()
        },
    )
    .await
}

/// The trips among `events`.
fn trips(events: &[TransactionEvent]) -> Vec<&CallIdBudgetTrip> {
    events
        .iter()
        .filter_map(|ev| match ev {
            TransactionEvent::CallIdBudgetTripped(trip) => Some(trip),
            _ => None,
        })
        .collect()
}

/// The requests among `events`.
fn requests(events: &[TransactionEvent]) -> usize {
    events
        .iter()
        .filter(|ev| {
            matches!(ev, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Request(_)))
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_call_id_past_its_budget_is_muted_both_ways_and_told_once() {
    let mut stack = stack(4).await;
    for branch in ["z9hG4bK-in-1", "z9hG4bK-in-2"] {
        stack.inject(&inbound_request("INFO", branch, "looping", Some("b2bua-tag"))).await;
    }
    elapse_ms(TRANSIT + 1).await;
    for _ in 0..3 {
        stack.txn.send_raw("looping", b"raw".to_vec(), addr(PEER)).await.expect("the layer runs");
    }
    stack.inject(&inbound_request("INFO", "z9hG4bK-in-3", "looping", Some("b2bua-tag"))).await;
    stack.txn.send_raw("elsewhere", b"other".to_vec(), addr(PEER)).await.expect("the layer runs");
    elapse_ms(TRANSIT + 1).await;

    let reached: Vec<Vec<u8>> =
        std::iter::from_fn(|| stack.peer.try_recv()).map(|p| p.raw.to_vec()).collect();
    assert_eq!(reached, [b"raw".to_vec(), b"raw".to_vec(), b"other".to_vec()], "the 5th is muted");
    let events = stack.drain_events();
    assert_eq!(requests(&events), 2, "the INFO after the trip is dropped before dispatch");
    let tripped = trips(&events);
    assert_eq!(tripped.len(), 1, "told once: {events:?}");
    assert_eq!(tripped[0].call_id, "looping");
    let metrics = stack.txn.metrics();
    assert_eq!(metrics.message_budget_trips(), 1);
    assert_eq!((metrics.muted_in(), metrics.muted_out()), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn a_trip_names_the_caller_of_a_tagless_invite_and_the_calls_of_its_transactions() {
    let mut stack = stack(6).await;
    stack.inject(&inbound_request("INVITE", "z9hG4bK-caller", "spiral", None)).await;
    elapse_ms(TRANSIT + 1).await;
    let dialled = invite_with_cr_lg("call-2", "spiral", "z9hG4bK-dialled", "b-1");
    stack.txn.send_request(dialled, addr(PEER), call::TxnKind::Invite).await.expect("sent");
    for _ in 0..6 {
        stack.txn.send_raw("spiral", b"raw".to_vec(), addr(PEER)).await.expect("the layer runs");
    }
    elapse_ms(TRANSIT + 1).await;

    let events = stack.drain_events();
    let tripped = trips(&events);
    assert_eq!(tripped.len(), 1, "{events:?}");
    assert_eq!(tripped[0].caller_tags, ["caller-tag"], "the caller's INVITE opened a dialog");
    assert_eq!(tripped[0].call_refs, ["call-2"], "the dialled INVITE's Via names its call");
}

#[tokio::test(start_paused = true)]
async fn out_of_dialog_options_on_the_keepalive_lane_spend_no_budget() {
    let mut stack = stack(1).await;
    for i in 0..3 {
        let branch = format!("z9hG4bK-probe-{i}");
        stack.inject(&inbound_request("OPTIONS", &branch, "prober", None)).await;
    }
    elapse_ms(TRANSIT + 1).await;
    let probes = stack.drain_events();
    assert_eq!(requests(&probes), 3, "every probe reaches the consumer");
    for probe in probes {
        let TransactionEvent::Message { message, .. } = probe else { continue };
        let SipMessage::Request(options) = *message else { continue };
        let ok =
            sip_message::generators::generate_response(&options, 200, "OK", &Default::default())
                .expect("an answer");
        stack.txn.send_response(ok, addr(PEER)).await.expect("the layer runs");
    }
    elapse_ms(TRANSIT + 1).await;

    assert_eq!(count_responses(&stack.drain_peer(), 200), 3, "every probe is answered");
    assert!(trips(&stack.drain_events()).is_empty());
    assert_eq!(stack.txn.metrics().message_budget_trips(), 0);
}

/// A datagram re-offered under a muted Call-ID is dropped as one from the
/// wire is: the transaction it matches does not take it and the consumer
/// hears nothing of it.
#[tokio::test(start_paused = true)]
async fn a_datagram_reoffered_under_a_muted_call_id_is_dropped() {
    let mut stack = stack(4).await;
    let branch = "z9hG4bK-muted-invite";
    stack.inject(&inbound_request("INVITE", branch, "muted", None)).await;
    elapse_ms(TRANSIT + 1).await;
    for _ in 0..4 {
        stack.txn.send_raw("muted", b"raw".to_vec(), addr(PEER)).await.expect("the layer runs");
    }
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(trips(&stack.drain_events()).len(), 1);
    let muted_in = stack.txn.metrics().muted_in();

    let cancel = CustomParser::new()
        .parse(&inbound_request("CANCEL", branch, "muted", None))
        .expect("parse");
    stack.txn.reoffer(cancel, addr(PEER)).await.expect("the layer runs");
    elapse_ms(TRANSIT + 1).await;
    assert!(stack.drain_events().is_empty(), "the INVITE is not cancelled");
    assert_eq!(stack.txn.metrics().muted_in(), muted_in + 1, "the mute dropped it");
}
