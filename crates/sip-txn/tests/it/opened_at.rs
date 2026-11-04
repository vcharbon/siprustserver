//! The event of a request that opened a server transaction states when it
//! opened: the request's receipt, whatever the event then waited behind. A
//! retransmission the transaction absorbs (RFC 3261 §17.2.3) is no new receipt
//! and draws no event.

use crate::common;
use common::*;
use sip_txn::TransactionEvent;
use tokio::time::Instant;

/// When the simulated fabric delivers a datagram injected now.
const TRANSIT_MS: u64 = 1;

/// The instant stated on the one event `events` holds.
fn stated(events: &[TransactionEvent]) -> Option<Instant> {
    assert_eq!(events.len(), 1, "one event: {events:?}");
    match &events[0] {
        TransactionEvent::Message { opened_at, .. } => *opened_at,
        other => panic!("a message: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_invite_states_its_receipt_however_late_its_event_is_taken() {
    let mut stack = Stack::build(TRANSIT_MS, 8, 1024).await;
    let invite = inbound_request("INVITE", "z9hG4bK-receipt", "receipt@unit", None);
    let sent = Instant::now();
    stack.inject(&invite).await;
    elapse_ms(3_000).await;
    let opened = stated(&stack.drain_events()).expect("it opened a transaction");
    assert!(opened.duration_since(sent).as_millis() <= u128::from(TRANSIT_MS) + 1, "{opened:?}");

    stack.inject(&invite).await;
    elapse_ms(100).await;
    assert!(stack.drain_events().is_empty(), "its retransmission draws no event");
}

#[tokio::test(start_paused = true)]
async fn a_response_opened_no_transaction_and_states_none() {
    let mut stack = Stack::build(TRANSIT_MS, 8, 1024).await;
    let ringing = response_bytes(180, "Ringing", "INVITE", "z9hG4bK-none", "none@unit", true);
    stack.inject(&ringing).await;
    elapse_ms(10).await;
    assert_eq!(stated(&stack.drain_events()), None);
}
