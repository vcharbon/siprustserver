//! `TransactionConfig::keepalive_lane`: an out-of-dialog OPTIONS passed up
//! statelessly rides a bounded lane of its own, so it reaches a consumer that
//! takes the lane first whatever the event queue holds; a full lane drops the
//! newest and counts it. With no lane the OPTIONS rides the event queue. The
//! age of the oldest event waiting in the queue is read at any instant.

use std::sync::Arc;
use std::time::Duration;

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::{EventQueueClass, IdGen, TransactionConfig, TransactionEvent};

const TRANSIT: u64 = 5;
/// The event queue's bound at `udp_queue_max` 16: the floor of 64.
const QUEUE: usize = 64;

async fn stack(lane: Option<usize>) -> Stack {
    Stack::build_with_config(
        TRANSIT,
        1024,
        TransactionConfig {
            udp_queue_max: 16,
            id_gen: Arc::new(IdGen::seeded(0x1a4e)),
            stateless_options: true,
            keepalive_lane: lane,
            ..common::config()
        },
    )
    .await
}

/// Fill the event queue with out-of-dialog MESSAGEs, one past its bound: the
/// last is dropped and leaves no transaction.
async fn fill_the_queue(stack: &Stack) {
    for i in 0..=QUEUE {
        stack.inject(&inbound_request("MESSAGE", &format!("z9hG4bK-fill-{i}"), "fill", None)).await;
    }
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(stack.txn.metrics().event_queue_depth(), QUEUE, "the event queue is full");
}

/// The method of a request event.
fn method(ev: &TransactionEvent) -> Option<String> {
    match ev {
        TransactionEvent::Message { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => Some(req.method().as_str().to_string()),
            SipMessage::Response(_) => None,
        },
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn an_options_reaches_the_lane_while_the_event_queue_is_full() {
    let mut stack = stack(Some(4)).await;
    let mut lane = stack.events.take_keepalive_lane().expect("the layer runs a lane");
    assert!(stack.events.take_keepalive_lane().is_none(), "the lane is handed over once");
    fill_the_queue(&stack).await;

    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-probe", "probe", None)).await;
    elapse_ms(TRANSIT + 1).await;
    let on_lane = lane.try_recv().expect("the OPTIONS is on the lane");
    assert_eq!(method(&on_lane).as_deref(), Some("OPTIONS"));
    assert_eq!(stack.txn.metrics().active_transactions(), QUEUE, "the OPTIONS opens none");
    assert_eq!(stack.txn.metrics().keepalive_lane_drops(), 0);
    let queued = stack.drain_events();
    assert!(queued.iter().all(|ev| method(ev).as_deref() == Some("MESSAGE")), "none queued");
}

#[tokio::test(start_paused = true)]
async fn a_full_lane_drops_the_newest_options_and_counts_it() {
    let mut stack = stack(Some(2)).await;
    let mut lane = stack.events.take_keepalive_lane().expect("the layer runs a lane");
    for i in 0..3 {
        stack
            .inject(&inbound_request("OPTIONS", &format!("z9hG4bK-probe-{i}"), "probe", None))
            .await;
    }
    elapse_ms(TRANSIT + 1).await;

    let mut branches = Vec::new();
    while let Ok(TransactionEvent::Message { message, .. }) = lane.try_recv() {
        if let SipMessage::Request(req) = *message {
            branches.push(req.top_via().branch().unwrap_or_default().to_string());
        }
    }
    assert_eq!(branches, ["z9hG4bK-probe-0", "z9hG4bK-probe-1"], "the first two, in order");
    assert_eq!(stack.txn.metrics().keepalive_lane_drops(), 1, "the newest is dropped, counted");
    assert!(stack.drain_events().is_empty(), "nothing reaches the event queue");
}

#[tokio::test(start_paused = true)]
async fn with_no_lane_the_options_rides_the_event_queue() {
    let mut stack = stack(None).await;
    assert!(stack.events.take_keepalive_lane().is_none(), "no lane");
    fill_the_queue(&stack).await;
    let dropped = stack.txn.metrics().event_queue_drops(EventQueueClass::RequestOther);

    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-probe", "probe", None)).await;
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(
        stack.txn.metrics().event_queue_drops(EventQueueClass::RequestOther),
        dropped + 1,
        "the full event queue drops it"
    );
    let _ = stack.drain_events();
    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-probe-2", "probe", None)).await;
    elapse_ms(TRANSIT + 1).await;
    let queued = stack.drain_events();
    assert_eq!(queued.iter().filter_map(method).collect::<Vec<_>>(), ["OPTIONS"], "queued");
}

#[tokio::test(start_paused = true)]
async fn the_oldest_waiting_event_ages_while_nothing_is_taken() {
    let mut stack = stack(None).await;
    let metrics = stack.txn.metrics().clone();
    assert_eq!(metrics.event_queue_delay(), Duration::ZERO, "an empty queue");
    for i in 0..2 {
        stack.inject(&inbound_request("MESSAGE", &format!("z9hG4bK-wait-{i}"), "wait", None)).await;
        elapse_ms(TRANSIT + 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    let first = metrics.event_queue_delay();
    assert!(
        first >= Duration::from_millis(1000) && first < Duration::from_millis(1010),
        "nothing taken: the first has waited a second: {first:?}"
    );

    stack.events.try_recv().expect("the first event");
    let second = metrics.event_queue_delay();
    assert_eq!(first - second, Duration::from_millis(106), "then the second, entered 106 ms later");
    stack.events.try_recv().expect("the second event");
    assert_eq!(metrics.event_queue_delay(), Duration::ZERO, "none waits");
}
