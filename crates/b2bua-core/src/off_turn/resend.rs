//! The requests a call awaits answers to, sent again from its record by the
//! node that now serves it (ADR-0039, ADR-0055 §1): the turn that sends them
//! retires the deadline its sender armed, and an admit the request carries is
//! numbered above every change the call reserved before
//! ([`call::CallLimiterState::renumber_consult`],
//! [`call::CallLimiterState::renumber_replacement`]), so the limiter checks it
//! net of what the key holds. Only an `Active` call sends again: an HTTP
//! request is made for its answer, and an ending call needs none.

use call::{Call, CallModelState, OffTurnRequest, TimerType};

use crate::effects::{CriticalStateEffect, FireAndForgetEffect, HandlerEffects, HandlerResult};
use crate::limiter::report::LimiterAdmitResult;

/// The turn that sends again every request `call` awaits, or those of `only`
/// among them; `None` when the call is not `Active` or awaits none of them.
pub fn fold(mut call: Call, only: Option<&[OffTurnRequest]>) -> Option<HandlerResult> {
    let (again, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut call.awaited)
        .into_iter()
        .partition(|r| only.is_none_or(|o| o.contains(r)));
    call.awaited = kept;
    if again.is_empty() || call.state != CallModelState::Active {
        return None;
    }
    let mut effects = HandlerEffects { resent: true, ..HandlerEffects::new() };
    for request in again {
        let retired =
            call::helpers::retire_timers(&mut call, |t| answers(&request, t.timer_type()));
        for entry in retired {
            effects.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
        }
        effects.fire_and_forget.push(sent_again(&mut call, request));
    }
    Some(HandlerResult { call, effects })
}

/// Whether `deadline` is the answer deadline of a request `call` awaits.
pub fn awaited_deadline(call: &Call, deadline: &TimerType) -> bool {
    call.awaited.iter().any(|r| answers(r, deadline))
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
        OffTurnRequest::Refer { request, .. } => {
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
        // A later change of the set superseded the replacement before it was
        // answered: it is answered so, and nothing is sent.
        OffTurnRequest::Admit { correlation_id, entries, change }
            if call.limiter.target_change() > change =>
        {
            let report = call::AdmitReport {
                key: call.limiter.key().to_string(),
                change,
                entries,
                outcome: call::AdmitOutcome::Superseded { held: call.limiter.held_set() },
            };
            let result = LimiterAdmitResult { call_ref, correlation_id, report };
            FireAndForgetEffect::Reenter(Box::new(result.into_event()))
        }
        OffTurnRequest::Admit { correlation_id, entries, .. } => {
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
