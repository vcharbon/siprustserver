//! Port of `tests/sip/transaction-layer-bounded-queue.test.ts` — the inbound→
//! app event queue is bounded; producers never block; excess is dropped-newest
//! and counted by reason; draining recovers normal accept.
//!
//! Adaptation: the source pumps incrementally so its tiny UDP recv queue
//! (`udpQueueMax`) never fills. Here the b2bua endpoint is bound with a
//! generous recv queue so the network layer never tail-drops, isolating the
//! event-queue bound under test. (The UDP-queue tail-drop is sip-net's own
//! concern, covered by its `simulated` tests.)

mod common;
use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest};
use sip_txn::{EventQueueDropReason, TransactionEvent};

const UDP_QUEUE_MAX: usize = 8; // event capacity = max(64, 4×8) = 64

#[tokio::test(start_paused = true)]
async fn capacity_and_counters_start_at_zero() {
    let stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_capacity(), std::cmp::max(64, UDP_QUEUE_MAX * 4));
    assert_eq!(m.event_queue_depth(), 0);
    for r in [
        EventQueueDropReason::Response,
        EventQueueDropReason::RequestInvite,
        EventQueueDropReason::RequestOther,
        EventQueueDropReason::Cancelled,
        EventQueueDropReason::Timeout,
    ] {
        assert_eq!(m.event_queue_drops(r), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn overflow_stops_at_capacity_and_counts_drops_then_recovers() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    let overflow = cap * 2;

    // No consumer drains `events`, so every parsed (unknown-branch) 180 lands
    // directly in the bounded queue; past `cap` they must drop, not block.
    for i in 0..overflow {
        stack
            .inject(&response_bytes(
                180,
                "Ringing",
                "INVITE",
                &format!("z9hG4bK-overflow-{i}"),
                &format!("overflow-{i}@unit"),
                true,
            ))
            .await;
    }

    // Auto-advance delivers all packets at the transit deadline; the owner
    // then stays continuously runnable (its recv queue is never empty) and so
    // drains the whole burst before the runtime goes idle and wakes us.
    elapse_ms(1_000).await;

    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_depth(), cap, "queue saturated at capacity");
    assert_eq!(m.event_queue_drops(EventQueueDropReason::Response), (overflow - cap) as u64);
    // Unrelated reasons untouched.
    assert_eq!(m.event_queue_drops(EventQueueDropReason::RequestInvite), 0);
    assert_eq!(m.event_queue_drops(EventQueueDropReason::Cancelled), 0);
    assert_eq!(m.event_queue_drops(EventQueueDropReason::Timeout), 0);

    // Drain `cap` events via the public receiver — queue empties to zero.
    for _ in 0..cap {
        stack.events.recv().await.expect("event");
    }
    assert_eq!(stack.txn.metrics().event_queue_depth(), 0);
}

/// The inbound requests of `method` among `events`.
fn requests_of(events: &[TransactionEvent], method: &str) -> Vec<SipRequest> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Fill the event queue to capacity with responses no transaction matches
/// (they build no transaction and are dropped-newest like any ordinary event).
async fn saturate(stack: &Stack, cap: usize) {
    for i in 0..cap {
        stack
            .inject(&response_bytes(
                180,
                "Ringing",
                "INVITE",
                &format!("z9hG4bK-fill-{i}"),
                &format!("fill-{i}@unit"),
                true,
            ))
            .await;
    }
    elapse_ms(100).await;
    assert_eq!(stack.txn.metrics().event_queue_depth(), cap, "queue saturated");
}

/// RFC 3261 §17.2.2: a non-INVITE request the queue could not take is not
/// admitted. Its retransmission (Timer E, §17.1.2.2) is the redelivery: it
/// reaches the consumer once the queue has room, draws the consumer's answer,
/// and a later copy replays that answer from the transaction.
#[tokio::test(start_paused = true)]
async fn a_non_invite_request_the_full_queue_dropped_is_admitted_on_its_retransmission() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let branch = "z9hG4bK-bye-dropped";
    let bye = inbound_request("BYE", branch, "bye-dropped@unit", Some("dlg-tag"));
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueDropReason::RequestOther), 1, "the drop is counted");
    assert_eq!(m.active_transactions(), 0, "a dropped request holds no server transaction");
    assert!(stack.drain_peer().is_empty(), "nothing answers a request the consumer never saw");

    // The consumer catches up; the peer's Timer E copy arrives.
    assert_eq!(stack.drain_events().len(), cap);
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let delivered = requests_of(&stack.drain_events(), "BYE");
    assert_eq!(delivered.len(), 1, "the retransmission reaches the consumer");

    // The consumer answers; a further copy replays the cached 200 (§17.2.2).
    let ok = generate_response(&delivered[0], 200, "OK", &GenerateResponseOpts::default());
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the BYE is answered");
    stack.inject(&bye).await;
    elapse_ms(100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "cached 200 replayed");
    assert!(stack.drain_events().is_empty(), "an answered copy does not re-surface");

    // Timer J ends the transaction.
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// Every copy the full queue refuses is dropped and counted on its own; the
/// first copy that finds room is admitted, once.
#[tokio::test(start_paused = true)]
async fn each_dropped_copy_is_counted_and_the_first_copy_with_room_is_admitted() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let bye = inbound_request("BYE", "z9hG4bK-bye-twice", "bye-twice@unit", Some("dlg-tag"));
    for _ in 0..2 {
        stack.inject(&bye).await;
        elapse_ms(100).await;
    }
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueDropReason::RequestOther), 2, "one per copy");
    assert_eq!(m.active_transactions(), 0);

    assert_eq!(stack.drain_events().len(), cap);
    stack.inject(&bye).await;
    elapse_ms(100).await;
    let delivered = requests_of(&stack.drain_events(), "BYE");
    assert_eq!(delivered.len(), 1, "the third copy reaches the consumer");
    assert_eq!(stack.txn.metrics().event_queue_drops(EventQueueDropReason::RequestOther), 2);

    let ok = generate_response(&delivered[0], 200, "OK", &GenerateResponseOpts::default());
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// A flood of distinct non-INVITE requests into a full queue leaves no
/// transaction behind: what the queue sheds costs the layer nothing.
#[tokio::test(start_paused = true)]
async fn non_invite_requests_shed_by_a_full_queue_leave_no_transaction() {
    let stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    let flood = 100;
    for i in 0..flood {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-flood-{i}"),
                &format!("flood-{i}@unit"),
                None,
            ))
            .await;
    }
    elapse_ms(100).await;
    let m = stack.txn.metrics();
    assert_eq!(m.event_queue_drops(EventQueueDropReason::RequestOther), flood as u64);
    assert_eq!(m.active_transactions(), 0);
    assert!(stack.drain_peer().is_empty());
}

/// Pin (holds without the non-INVITE fix): an INVITE whose 100 Trying already
/// silenced the caller's retransmission is its own only delivery (RFC 3261
/// §17.2.1), so a full queue defers it, and it reaches the consumer once the
/// queue has room.
#[tokio::test(start_paused = true)]
async fn an_invite_the_full_queue_could_not_take_is_deferred_not_dropped() {
    let mut stack = Stack::build(1, UDP_QUEUE_MAX, 1024).await;
    let cap = stack.txn.metrics().event_queue_capacity();
    saturate(&stack, cap).await;

    stack.inject(&inbound_request("INVITE", "z9hG4bK-inv-deferred", "inv@unit", None)).await;
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the 100 Trying left at once");
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the INVITE holds its transaction");

    assert_eq!(stack.drain_events().len(), cap, "the INVITE is not in the saturated queue");
    elapse_ms(150).await;
    let delivered = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(delivered.len(), 1, "the deferred INVITE is delivered once there is room");

    // The consumer rejects it; the caller's ACK ends the transaction (§17.2.1).
    let busy = generate_response(
        &delivered[0],
        486,
        "Busy Here",
        &GenerateResponseOpts { to_tag: Some("uas-tag".into()), ..Default::default() },
    );
    stack.txn.send_response(busy, addr(PEER)).await.unwrap();
    elapse_ms(50).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1);
    stack
        .inject(&inbound_request("ACK", "z9hG4bK-inv-deferred", "inv@unit", Some("uas-tag")))
        .await;
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}
