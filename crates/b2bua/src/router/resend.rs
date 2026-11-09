//! The requests a call awaits answers to, sent again by the node that now
//! serves it (ADR-0039, ADR-0055 §1): the task that awaited each answer died
//! with the node that sent the request, and the record holds the request as
//! its turn sent it. Every materialisation that serves the call queues this
//! turn; it runs on the call's own queue, behind any message already queued,
//! and lands as any turn does, so the write precedes the sends, each deadline
//! is armed afresh from the re-send, and the requests leave through the
//! call's outbox. What is sent again is the core's fold
//! ([`crate::off_turn::resend`]).

use std::sync::Arc;

use call::{Call, OffTurnRequest};

use super::commit::commit_turn;
use super::RouterCtx;
use crate::off_turn::resend;
use b2bua_sdk::event::CallEvent;

/// The topic of the re-send notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
pub(super) const TOPIC: &str = "off-turn-resend";

/// The re-send notice of `call`, served here from its record with requests
/// still awaited.
pub(super) fn notice(call: &Call) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: Some(call.incarnation().to_string()),
    }
}

/// The re-send notice of `call` for the requests of `only` alone, which this
/// node took over from another owner's record while it awaits the rest itself.
pub(super) fn notice_of_only(call: &Call, only: &[OffTurnRequest]) -> CallEvent {
    let mut event = notice(call);
    if let CallEvent::InternalEvent { payload, .. } = &mut event {
        *payload = serde_json::to_value(only).unwrap_or(serde_json::Value::Null);
    }
    event
}

/// Whether `event` is the re-send notice ([`notice`]), of which call, and the
/// requests it sends again alone, if it names them ([`notice_of_only`]).
pub(super) fn notice_of(event: &CallEvent) -> Option<(&str, Option<Vec<OffTurnRequest>>)> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, payload, .. } if topic == TOPIC => {
            Some((call_ref, serde_json::from_value(payload.clone()).ok()))
        }
        _ => None,
    }
}

/// The re-send turn of `call_ref`, under its per-call lock: every request the
/// resident call awaits, or those of `only` among them, is sent again
/// ([`resend::fold`]). A call gone, ending or awaiting none of them sends
/// nothing.
pub(super) async fn resend_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    only: Option<Vec<OffTurnRequest>>,
) {
    let _guard = ctx.state.lock(call_ref).await;
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let Some(result) = resend::fold(call, only.as_deref()) else { return };
    tracing::info!(
        node = observe::node(),
        call_ref,
        requests = result.effects.fire_and_forget.len(),
        "awaited requests sent again"
    );
    let now_ms = ctx.clock.now_ms();
    let turn = &"the re-send of the requests the call awaits";
    commit_turn(ctx, call_ref, result, now_ms, turn, None).await;
}

#[cfg(test)]
mod tests;
