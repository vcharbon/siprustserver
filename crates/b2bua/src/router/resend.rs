//! The requests a call awaits answers to, sent again by the node that now
//! serves it (ADR-0039, ADR-0055 §1): the task that awaited each answer died
//! with the node that sent the request, and the record holds the request as
//! its turn sent it. Every materialisation that serves the call queues this
//! turn; it runs on the call's own queue, behind any message already queued,
//! and lands as any turn does, so the write precedes the sends, each deadline
//! is armed afresh from the re-send, and the requests leave through the
//! call's outbox. An admit the request carries is numbered above every change
//! the call reserved before ([`call::CallLimiterState::renumber_consult`],
//! [`call::CallLimiterState::renumber_replacement`]): the limiter checks it
//! net of what the key holds, so a set the first request admitted is not
//! counted twice.

use std::sync::Arc;

use call::{Call, CallModelState, OffTurnRequest, TimerType};

use super::interpret::process_result;
use super::RouterCtx;
use crate::effects::{CriticalStateEffect, FireAndForgetEffect, HandlerEffects, HandlerResult};
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

/// Whether `event` is the re-send notice ([`notice`]), and of which call.
pub(super) fn notice_of(event: &CallEvent) -> Option<&str> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, .. } if topic == TOPIC => Some(call_ref),
        _ => None,
    }
}

/// The re-send turn of `call_ref`, under its per-call lock: every request the
/// resident call awaits is sent again, the deadline its sender armed retired.
/// A call gone, ended or awaiting nothing sends nothing.
pub(super) async fn resend_turn(ctx: &Arc<RouterCtx>, call_ref: &str) {
    let _guard = ctx.state.lock(call_ref).await;
    let Some(mut call) = ctx.state.peek(call_ref) else { return };
    if call.awaited.is_empty() || call.state == CallModelState::Terminated {
        return;
    }
    let now_ms = ctx.clock.now_ms();
    let mut effects = HandlerEffects::new();
    for request in std::mem::take(&mut call.awaited) {
        let retired =
            call::helpers::retire_timers(&mut call, |t| answers(&request, t.timer_type()));
        for entry in retired {
            effects.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
        }
        effects.fire_and_forget.push(sent_again(&mut call, request));
    }
    tracing::info!(
        node = observe::node(),
        call_ref,
        requests = effects.fire_and_forget.len(),
        "awaited requests sent again"
    );
    let result = HandlerResult { call, effects };
    let turn = &"the re-send of the requests the call awaits";
    process_result(ctx, call_ref, result, now_ms, turn, None).await;
}

/// Whether `deadline` is the answer deadline of `request`.
fn answers(request: &OffTurnRequest, deadline: &TimerType) -> bool {
    match (request, deadline) {
        (
            OffTurnRequest::ServiceHttp { correlation_id: a, .. },
            TimerType::ServiceHttpAnswer { correlation_id: b },
        )
        | (
            OffTurnRequest::Admit { correlation_id: a, .. },
            TimerType::ServiceAdmitAnswer { correlation_id: b, .. },
        ) => a == b,
        (OffTurnRequest::Failure { limiter, .. }, TimerType::FailureAnswer { change, .. })
        | (OffTurnRequest::Release { limiter, .. }, TimerType::ReleaseAnswer { change }) => {
            limiter.first_change == *change
        }
        _ => false,
    }
}

/// The effect that sends `request` of `call` again, its admits numbered
/// afresh.
fn sent_again(call: &mut Call, request: OffTurnRequest) -> FireAndForgetEffect {
    let call_ref = call.call_ref.clone();
    match request {
        OffTurnRequest::Refer { request } => {
            FireAndForgetEffect::ReferAsyncHttp { call_ref, request }
        }
        OffTurnRequest::ServiceHttp {
            correlation_id,
            endpoint,
            method,
            headers,
            body,
            content_type,
            timeout_ms,
        } => FireAndForgetEffect::ServiceHttpRequest {
            call_ref,
            correlation_id,
            endpoint,
            method,
            headers,
            body,
            content_type,
            timeout_ms,
        },
        OffTurnRequest::Failure { request, .. } => {
            let chain = u64::from(crate::answer_deadline::FAILURE_CHAIN);
            let limiter = call.limiter.renumber_consult(chain);
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request, limiter, deadline: None }
        }
        OffTurnRequest::Release { request, .. } => {
            let limiter = call.limiter.renumber_consult(1);
            FireAndForgetEffect::ReleaseAsyncHttp { call_ref, request, limiter, deadline: None }
        }
        OffTurnRequest::Admit { correlation_id, entries } => {
            let (change, held) = call.limiter.renumber_replacement(entries.clone());
            let key = call.limiter.key().to_string();
            FireAndForgetEffect::LimiterAdmit {
                call_ref,
                correlation_id,
                key,
                change,
                held,
                entries,
            }
        }
    }
}

#[cfg(test)]
mod tests;
