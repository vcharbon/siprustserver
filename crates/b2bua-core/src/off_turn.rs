//! The record of the requests a call sends off its turn while their answers
//! are awaited ([`call::OffTurnRequest`], ADR-0039, ADR-0055 §1): the landing
//! pass writes each request the turn sends into the record it lands, so the
//! write carries the request before the request leaves, and the answer, or
//! its deadline's expiry, takes it off before the rules read the event. A
//! node that serves the call from the record sends what is left again.

use call::{Call, CallModelState, OffTurnRequest, TimerType};

use crate::answer_deadline::CONSULT_CHANGE;
use crate::decision_log::{FAILURE_TOPIC, REFER_TOPIC};
use crate::effects::{FireAndForgetEffect, HandlerResult};
use crate::limiter::report::LimiterAdmitResult;
use b2bua_sdk::event::CallEvent;

/// The topic of an adaptation HTTP request's result.
const SERVICE_HTTP_RESULT: &str = "service-http-result";

/// The topic of a release event's consult fold.
const RELEASE_TOPIC: &str = "call-release-result";

/// `result` with every request its turn sends off the call's turn on its
/// record, replacing the same request sent before. A turn that ends the call
/// awaits nothing.
pub fn record(mut result: HandlerResult, now_ms: i64) -> HandlerResult {
    if result.call.state == CallModelState::Terminated {
        result.call.awaited.clear();
        return result;
    }
    for fx in &result.effects.fire_and_forget {
        let Some(request) = image_of(fx, now_ms) else { continue };
        result.call.awaited.retain(|r| !r.same_request(&request));
        result.call.awaited.push(request);
    }
    result
}

/// The record image of `fx`, a request sent off the call's turn at
/// `now_ms`; `None` for an answer the turn re-enters itself.
fn image_of(fx: &FireAndForgetEffect, now_ms: i64) -> Option<OffTurnRequest> {
    Some(match fx {
        FireAndForgetEffect::ReferAsyncHttp { request, .. } => {
            OffTurnRequest::Refer { request: request.clone(), sent_at_ms: now_ms }
        }
        FireAndForgetEffect::ServiceHttpRequest {
            correlation_id,
            endpoint,
            method,
            headers,
            body,
            content_type,
            timeout_ms,
            ..
        } => OffTurnRequest::ServiceHttp {
            correlation_id: correlation_id.clone(),
            endpoint: endpoint.clone(),
            method: method.clone(),
            headers: headers.clone(),
            body: body.clone(),
            content_type: content_type.clone(),
            timeout_ms: *timeout_ms,
        },
        FireAndForgetEffect::FailureAsyncHttp { request, limiter, .. } => {
            OffTurnRequest::Failure { request: request.clone(), limiter: *limiter }
        }
        FireAndForgetEffect::ReleaseAsyncHttp { request, limiter, .. } => {
            OffTurnRequest::Release { request: request.clone(), limiter: *limiter }
        }
        FireAndForgetEffect::LimiterAdmit { correlation_id, entries, change, .. } => {
            OffTurnRequest::Admit {
                correlation_id: correlation_id.clone(),
                entries: entries.clone(),
                change: *change,
            }
        }
        FireAndForgetEffect::Reenter(_) => return None,
    })
}

/// Whether `event` is the answer to a request a call sent off its turn: what
/// only the node that sent the request receives, for the call it then held.
pub fn is_answer(event: &CallEvent) -> bool {
    matches!(event, CallEvent::InternalEvent { topic, .. }
        if matches!(topic.as_str(), REFER_TOPIC | SERVICE_HTTP_RESULT | FAILURE_TOPIC | RELEASE_TOPIC)
            || topic == LimiterAdmitResult::TOPIC)
}

/// Take off `call`'s record the request `event` answers: its answer, or the
/// expiry of its deadline. A consult fold that names no deadline (a consult
/// with no deadline) answers every consult of its kind.
pub fn settle(call: &mut Call, event: &CallEvent) {
    let answered: Box<dyn Fn(&OffTurnRequest) -> bool> = match event {
        CallEvent::Timer { key, .. } => match &key.timer_type {
            TimerType::ServiceHttpAnswer { correlation_id } => {
                let id = correlation_id.clone();
                Box::new(
                    move |r| matches!(r, OffTurnRequest::ServiceHttp { correlation_id, .. } if *correlation_id == id),
                )
            }
            TimerType::ServiceAdmitAnswer { correlation_id, .. } => {
                let id = correlation_id.clone();
                Box::new(
                    move |r| matches!(r, OffTurnRequest::Admit { correlation_id, .. } if *correlation_id == id),
                )
            }
            TimerType::FailureAnswer { change, .. } => {
                let change = *change;
                Box::new(
                    move |r| matches!(r, OffTurnRequest::Failure { limiter, .. } if limiter.first_change == change),
                )
            }
            TimerType::ReleaseAnswer { change } => {
                let change = *change;
                Box::new(
                    move |r| matches!(r, OffTurnRequest::Release { limiter, .. } if limiter.first_change == change),
                )
            }
            _ => return,
        },
        CallEvent::InternalEvent { topic, payload, .. } => {
            let correlation = payload.get("correlation_id").and_then(|v| v.as_str());
            let consult = payload.get(CONSULT_CHANGE).and_then(serde_json::Value::as_u64);
            match topic.as_str() {
                REFER_TOPIC => Box::new(|r| matches!(r, OffTurnRequest::Refer { .. })),
                SERVICE_HTTP_RESULT => {
                    let id = correlation.unwrap_or_default().to_string();
                    Box::new(
                        move |r| matches!(r, OffTurnRequest::ServiceHttp { correlation_id, .. } if *correlation_id == id),
                    )
                }
                LimiterAdmitResult::TOPIC => {
                    let id = correlation.unwrap_or_default().to_string();
                    Box::new(
                        move |r| matches!(r, OffTurnRequest::Admit { correlation_id, .. } if *correlation_id == id),
                    )
                }
                FAILURE_TOPIC => Box::new(move |r| match r {
                    OffTurnRequest::Failure { limiter, .. } => {
                        consult.is_none_or(|c| c == limiter.first_change)
                    }
                    _ => false,
                }),
                RELEASE_TOPIC => Box::new(move |r| match r {
                    OffTurnRequest::Release { limiter, .. } => {
                        consult.is_none_or(|c| c == limiter.first_change)
                    }
                    _ => false,
                }),
                _ => return,
            }
        }
        _ => return,
    };
    call.awaited.retain(|r| !answered(r));
}

#[cfg(test)]
mod tests;
