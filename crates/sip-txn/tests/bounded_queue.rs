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
use sip_txn::EventQueueDropReason;

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
    assert_eq!(
        m.event_queue_drops(EventQueueDropReason::Response),
        (overflow - cap) as u64
    );
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
