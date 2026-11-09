//! A replicated flush reaches its call as a turn of the call's own queue. The
//! reconcile of a reverse flush and the meeting of a refused flush need the
//! per-call lock, which a call's own turn holds across its decision round trip:
//! the router loop only queues them, so it never waits on a third party for
//! them (ADR-0033 decision 5), and a message queued for the call before the
//! flush is served first (ADR-0010 X2).
//!
//! A turn that leaves no call live here and nothing queued behind it releases
//! the queue. With an item behind it the queue stays: that item may make the
//! call live, and its own turn releases what it leaves.

use std::sync::Arc;

use super::reclaim::{
    meet_refused_flush, reconcile_reverse_flush, refused_flush_may_meet, FlushDirection,
};
use super::release::{release_call, ReleaseKind};
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// The topic of a flush's notice, an internal event the router raises itself
/// and its turn consumes before any rule reads it.
const TOPIC: &str = "replicated-flush";
/// The notice's outcome for a reverse flush the store took.
const RECONCILE: &str = "reconcile";
/// The notice's outcome for a refused reverse flush; its body rides the event.
const REFUSED_REVERSE: &str = "refused-reverse";
/// The notice's outcome for a refused forward flush; its body rides the event.
const REFUSED_FORWARD: &str = "refused-forward";

/// Queue the reconcile of the reverse flush of `call_ref` the store took.
pub(super) async fn offer_reconcile(ctx: &Arc<RouterCtx>, call_ref: String) {
    let event = notice(call_ref, RECONCILE, 0, Vec::new());
    super::ingress::on_event(ctx, event, Default::default(), None).await;
}

/// Queue the meeting of the refused flush `body` of `call_ref`, flushed at
/// `origin_now_ms` on the flushing node's clock; nothing for an Element with no
/// copy here to meet, the common case, which opens no queue.
pub(super) async fn offer_refused(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: String,
    body: &[u8],
    origin_now_ms: i64,
) {
    if !refused_flush_may_meet(ctx, direction, &call_ref) {
        return;
    }
    let outcome = match direction {
        FlushDirection::Reverse => REFUSED_REVERSE,
        FlushDirection::Forward => REFUSED_FORWARD,
    };
    let event = notice(call_ref, outcome, origin_now_ms, body.to_vec());
    super::ingress::on_event(ctx, event, Default::default(), None).await;
}

fn notice(call_ref: String, outcome: &str, origin_now_ms: i64, body: Vec<u8>) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref,
        topic: TOPIC.to_string(),
        outcome: outcome.to_string(),
        payload: serde_json::json!(origin_now_ms),
        body,
        incarnation: None,
    }
}

/// Whether `event` is a flush's notice.
pub(super) fn is_notice(event: &CallEvent) -> bool {
    matches!(event, CallEvent::InternalEvent { topic, .. } if topic == TOPIC)
}

/// The turn of a flush's notice `event`.
pub(super) async fn flush_turn(ctx: &Arc<RouterCtx>, event: &CallEvent) {
    let CallEvent::InternalEvent { call_ref, outcome, payload, body, .. } = event else {
        return;
    };
    let origin_now_ms = payload.as_i64().unwrap_or_default();
    match outcome.as_str() {
        RECONCILE => reconcile_reverse_flush(ctx, call_ref).await,
        REFUSED_REVERSE => {
            meet_refused_flush(ctx, FlushDirection::Reverse, call_ref, body, origin_now_ms).await
        }
        REFUSED_FORWARD => {
            meet_refused_flush(ctx, FlushDirection::Forward, call_ref, body, origin_now_ms).await
        }
        _ => {}
    }
    if !ctx.state.contains(call_ref) && ctx.dispatcher.is_idle(call_ref) {
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
    }
}
