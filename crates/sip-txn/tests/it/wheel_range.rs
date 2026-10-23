//! A configured bound past what the timer wheel holds, or a receive queue
//! past what a channel holds, is held at its ceiling: the layer is spawned,
//! its owner loop keeps turning, and every transaction still completes.

use std::sync::Arc;

use call::TxnKind;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::SipMessage;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};

use crate::common;
use common::*;

/// An INVITE bound no timer wheel holds arms at the ceiling, for a server
/// INVITE and a client INVITE alike: each is answered with a final and
/// leaves the map at its hold.
#[tokio::test(start_paused = true)]
async fn an_invite_bound_past_the_wheel_is_held_at_its_ceiling() {
    let config = TransactionConfig { invite_initial_timeout_ms: u64::MAX, ..common::config() };
    let mut stack = Stack::build_with_config(1, 64, config).await;

    stack.inject(&inbound_request("INVITE", "z9hG4bK-far-bound", "far-bound@u", None)).await;
    let handle = stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-far-client"), addr(PEER), TxnKind::Invite)
        .await
        .expect("the owner takes the client INVITE");
    elapse_ms(10).await;
    assert!(!stack.txn.owner_task().is_finished(), "the owner outlives the far bound");

    let invite = first_request(stack.drain_events()).expect("the server INVITE reaches the TU");
    let opts = GenerateResponseOpts { to_tag: Some("far-uas".into()), ..Default::default() };
    let busy = generate_response(&invite, 486, "Busy Here", &opts).expect("a 486");
    stack.txn.send_response(busy, addr(PEER)).await.expect("the 486 leaves");
    stack
        .inject(&inbound_request("ACK", "z9hG4bK-far-bound", "far-bound@u", Some("far-uas")))
        .await;
    let client_busy =
        response_bytes(486, "Busy Here", "INVITE", handle.branch(), "handle-shape-test", true);
    stack.inject(&client_busy).await;

    elapse_ms(40_000).await;
    assert!(!stack.txn.owner_task().is_finished());
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "both left at their holds");
}

/// A receive queue no channel holds sizes the event queue at its ceiling.
#[tokio::test(start_paused = true)]
async fn a_receive_queue_past_a_channel_is_held_at_its_ceiling() {
    let config = TransactionConfig {
        udp_queue_max: usize::MAX,
        ..TransactionConfig::new(Arc::new(IdGen::seeded(1)))
    };
    assert_eq!(config.event_capacity(), 1 << 20);
    let mut stack = Stack::build_with_config(1, 64, config).await;
    assert_eq!(stack.txn.metrics().event_queue_capacity(), 1 << 20);

    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-wide", "wide@u", None)).await;
    elapse_ms(10).await;
    let options = first_request(stack.drain_events()).expect("the OPTIONS reaches the TU");
    let ok =
        generate_response(&options, 200, "OK", &GenerateResponseOpts::default()).expect("a 200");
    stack.txn.send_response(ok, addr(PEER)).await.expect("the 200 leaves");
    elapse_ms(40_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the peer has its final");
    assert!(!stack.txn.owner_task().is_finished());
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "it left at Timer J");
}

/// The first request among `events`.
fn first_request(events: Vec<TransactionEvent>) -> Option<sip_message::SipRequest> {
    events.into_iter().find_map(|e| match e {
        TransactionEvent::Message { message, .. } => match *message {
            SipMessage::Request(r) => Some(r),
            SipMessage::Response(_) => None,
        },
        _ => None,
    })
}
