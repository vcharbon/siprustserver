//! The subscriptions this stack notifies on a dialog and its requests queued
//! there, for the round-trip property.

use call::model::*;
use proptest::prelude::*;

use super::{arb_bytes, arb_tag};

/// Any request of this stack waiting on a dialog, behind what it waits.
pub(super) fn arb_queued() -> impl Strategy<Value = Queued> {
    let behind = prop_oneof![Just(Behind::AnswerAck), Just(Behind::OwnNotify)];
    (behind, arb_queued_request()).prop_map(|(behind, request)| Queued { behind, request })
}

/// Any request of this stack queued on a dialog.
fn arb_queued_request() -> impl Strategy<Value = QueuedRequest> {
    let headers = proptest::collection::vec((arb_tag(), arb_tag()), 0..3);
    prop_oneof![
        (arb_tag(), arb_tag(), proptest::option::of(arb_tag()), arb_bytes(80)).prop_map(
            |(event, subscription_state, content_type, body)| QueuedRequest::Notify {
                event,
                subscription_state,
                content_type,
                body,
            }
        ),
        (
            proptest::option::of(arb_tag()),
            headers,
            arb_bytes(80),
            proptest::option::of(arb_tag()),
            proptest::option::of(any::<u32>()),
        )
            .prop_map(|(reason, headers, body, content_type, max_forwards)| {
                QueuedRequest::Bye { reason, headers, body, content_type, max_forwards }
            }),
    ]
}

/// Any subscription this stack notifies, its NOTIFY in flight or not.
pub(super) fn arb_subscription() -> impl Strategy<Value = Subscription> {
    let state = prop_oneof![
        Just(SubscriptionState::Pending),
        Just(SubscriptionState::Active),
        Just(SubscriptionState::Terminated),
    ];
    let notify = proptest::option::of(
        (any::<i64>(), any::<i64>())
            .prop_map(|(cseq, sent_at_ms)| NotifyInFlight { cseq, sent_at_ms }),
    );
    (arb_tag(), proptest::option::of(arb_tag()), state, notify)
        .prop_map(|(event, id, state, notify)| Subscription { event, id, state, notify })
}
