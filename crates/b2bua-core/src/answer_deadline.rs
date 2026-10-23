//! The deadline of the answer to a request sent off the call's turn: a
//! service's adaptation HTTP request (`RuleAction::ServiceHttpRequest`) or
//! replacement of the call's admission set (`RuleAction::ReplaceAdmissionSet`),
//! and the core's `/call/failure` and release-event consults
//! (`RuleAction::FailureAsyncHttp`, `RuleAction::ReleaseAsyncHttp`).
//! The task awaiting the answer lives on the node that sent the request and
//! dies with it; the deadline lives in the call's replicated timer ledger and
//! is re-armed wherever the call is materialised, so a call taken over or
//! reclaimed elsewhere reads the lost answer at the deadline (ADR-0039).
//!
//! - [`arm`]: the turn that sends a request arms its deadline at the request's
//!   budget plus [`MARGIN`]; the router answers every request by its budget,
//!   so the deadline never fires before a live answer.
//! - [`screen`], before the rules read an event: an answer whose deadline is
//!   in the ledger cancels it and reaches the rules; an answer whose deadline
//!   is not (it expired) reaches none, its admit report still applied; so does
//!   an admit report under another limiter key (an earlier call under the
//!   same `call_ref`), which leaves the call's own deadline awaited. The
//!   deadline's expiry — a fire the router's timer screen delivered, so its
//!   answer had not come first — is the request's answer with nothing usable:
//!   an HTTP request's `error` result ([`NO_ANSWER`]), an admit's
//!   `unavailable` report, a failure consult's unanswered `503` fold. A
//!   release consult's expiry is the release event again, which its rule
//!   consults on anew.

use std::time::Duration;

use call::{Call, CallModelState, TimerKey, TimerType};

use crate::config::B2buaConfig;
use crate::decision_log::FAILURE_TOPIC;
use crate::effects::{CriticalStateEffect, FireAndForgetEffect, HandlerEffects, HandlerResult};
use crate::limiter::report::LimiterAdmitResult;
use crate::limiter::ADMIT_SLACK;
use b2bua_sdk::event::CallEvent;

/// How long past its budget a request's answer is still awaited: the answer's
/// way back through the node's queues on a healthy node.
pub const MARGIN: Duration = Duration::from_secs(2);

// The admit bound's slack lies well inside the margin.
#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(ADMIT_SLACK.as_millis() * 4 <= MARGIN.as_millis());

/// The `/call/failure` consults one failover chain may send: the first, and
/// one per limiter refusal it re-consults on.
pub const FAILURE_CHAIN: u32 = crate::decision::MAX_LIMITER_FAILOVER + 1;

/// The budget a `/call/failure` consult is answered in, `decision` being the
/// decision engine's per-consult deadline and `admit` the limiter's admit
/// budget: each consult of the chain, then its route's admit (bounded just
/// past the admit budget). `None` when the decision deadline is off
/// (`decision` zero): the consult is unbounded and has no deadline.
pub fn failure_budget(decision: Duration, admit: Duration) -> Option<Duration> {
    (!decision.is_zero()).then(|| (decision + admit + ADMIT_SLACK) * FAILURE_CHAIN)
}

/// The budget a release event's consult is answered in: the consult, then
/// the admit of the route it may answer (bounded just past the admit
/// budget). `None` when the decision deadline is off (`decision` zero).
pub fn release_budget(decision: Duration, admit: Duration) -> Option<Duration> {
    (!decision.is_zero()).then(|| decision + admit + ADMIT_SLACK)
}

/// [`release_budget`] under `config`'s decision deadline.
pub fn release_budget_under(config: &B2buaConfig, admit: Duration) -> Option<Duration> {
    release_budget(decision_deadline(config), admit)
}

/// `config`'s decision deadline, zero when not positive.
fn decision_deadline(config: &B2buaConfig) -> Duration {
    Duration::from_millis(u64::try_from(config.call_control_timeout_ms).unwrap_or(0))
}

/// [`failure_budget`] under `config`'s decision deadline
/// (`call_control_timeout_ms`, off when not positive).
pub fn failure_budget_under(config: &B2buaConfig, admit: Duration) -> Option<Duration> {
    failure_budget(decision_deadline(config), admit)
}

/// The payload key a `/call/failure` fold states the deadline it answers
/// under (the consult's first change number), as [`arm`] named it on the
/// effect. A consult with no deadline states none, and its fold reaches the
/// rules unscreened.
pub const CONSULT_CHANGE: &str = "consult_change";

/// The `error` an HTTP request's result states when its deadline expired.
pub const NO_ANSWER: &str = "no_answer";

/// The topic of an adaptation HTTP request's result.
const SERVICE_HTTP_RESULT: &str = "service-http-result";

/// The topic of a release event's consult fold.
const RELEASE_TOPIC: &str = "call-release-result";

/// The budgets the router answers a service's requests by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerBudgets {
    /// An HTTP request that states no `timeout_ms` of its own.
    pub http: Duration,
    /// An admit's budget.
    pub admit: Duration,
    /// A `/call/failure` consult's chain ([`failure_budget`]); `None` when
    /// the consult is unbounded.
    pub failure: Option<Duration>,
    /// A release event's consult ([`release_budget`]); `None` when the
    /// consult is unbounded.
    pub release: Option<Duration>,
}

/// `result` with the deadline of every request its turn sends off the call's
/// turn — a service's request, a `/call/failure` consult — armed on the
/// record and scheduled, `now_ms` being the turn's time; a failure consult's
/// effect names the deadline armed for it, the one place that decides it. A
/// turn that ends the call arms nothing.
pub fn arm(mut result: HandlerResult, budgets: &AnswerBudgets, now_ms: i64) -> HandlerResult {
    if result.call.state == CallModelState::Terminated {
        return result;
    }
    let deadlines: Vec<(TimerType, Duration)> =
        result.effects.fire_and_forget.iter().filter_map(|fx| deadline_of(fx, budgets)).collect();
    for fx in &mut result.effects.fire_and_forget {
        match fx {
            FireAndForgetEffect::FailureAsyncHttp { limiter, deadline, .. } => {
                *deadline = budgets.failure.map(|_| limiter.first_change);
            }
            FireAndForgetEffect::ReleaseAsyncHttp { limiter, deadline, .. } => {
                *deadline = budgets.release.map(|_| limiter.first_change);
            }
            _ => {}
        }
    }
    for (timer_type, budget) in deadlines {
        let fire_at = now_ms.saturating_add(call::millis_i64(budget.saturating_add(MARGIN)));
        crate::timer_arm::arm(
            &mut result.call,
            &mut result.effects,
            TimerKey::call(timer_type),
            fire_at,
        );
    }
    result
}

/// The deadline a sent effect awaits its answer by, and the budget it is
/// answered in: a request's, or none for an answer the turn re-enters itself
/// (an admit that sent nothing).
fn deadline_of(fx: &FireAndForgetEffect, budgets: &AnswerBudgets) -> Option<(TimerType, Duration)> {
    match fx {
        FireAndForgetEffect::ServiceHttpRequest { correlation_id, timeout_ms, .. } => Some((
            TimerType::ServiceHttpAnswer { correlation_id: correlation_id.clone() },
            timeout_ms.map(Duration::from_millis).unwrap_or(budgets.http),
        )),
        FireAndForgetEffect::LimiterAdmit { correlation_id, change, .. } => Some((
            TimerType::ServiceAdmitAnswer {
                correlation_id: correlation_id.clone(),
                change: *change,
            },
            budgets.admit,
        )),
        FireAndForgetEffect::FailureAsyncHttp { request, limiter, .. } => {
            budgets.failure.map(|budget| {
                let unanswered = crate::failure_unavailable::payload(request);
                (TimerType::FailureAnswer { change: limiter.first_change, unanswered }, budget)
            })
        }
        FireAndForgetEffect::ReleaseAsyncHttp { limiter, .. } => budgets
            .release
            .map(|budget| (TimerType::ReleaseAnswer { change: limiter.first_change }, budget)),
        FireAndForgetEffect::Reenter(event) => {
            answered(event).map(|timer_type| (timer_type, Duration::ZERO))
        }
        _ => None,
    }
}

/// The deadline a service request's answer closes: its result naming its
/// correlation id. An admit's deadline is matched on its correlation id alone
/// ([`closes`]).
fn answered(event: &CallEvent) -> Option<TimerType> {
    let CallEvent::InternalEvent { topic, payload, .. } = event else {
        return None;
    };
    let correlation_id = payload.get("correlation_id")?.as_str()?.to_string();
    if topic == SERVICE_HTTP_RESULT {
        Some(TimerType::ServiceHttpAnswer { correlation_id })
    } else if topic == LimiterAdmitResult::TOPIC {
        let change = crate::limiter::report::admit_report_of(event).map_or(0, |r| r.change);
        Some(TimerType::ServiceAdmitAnswer { correlation_id, change })
    } else {
        None
    }
}

/// The deadline a `/call/failure` or release fold closes: its consult's first
/// change number ([`CONSULT_CHANGE`]), matched on that alone ([`closes`]).
fn consult_answered(event: &CallEvent) -> Option<TimerType> {
    let CallEvent::InternalEvent { topic, payload, .. } = event else {
        return None;
    };
    let change = payload.get(CONSULT_CHANGE)?.as_u64()?;
    if topic == FAILURE_TOPIC {
        Some(TimerType::FailureAnswer { change, unanswered: serde_json::Value::Null })
    } else if topic == RELEASE_TOPIC {
        Some(TimerType::ReleaseAnswer { change })
    } else {
        None
    }
}

/// Whether the armed deadline `armed` is the one `answer` closes: an HTTP
/// request's by its correlation id, an admit's by its correlation id, a
/// failure consult's by its change number.
fn closes(armed: &TimerType, answer: &TimerType) -> bool {
    match (armed, answer) {
        (
            TimerType::ServiceHttpAnswer { correlation_id: a },
            TimerType::ServiceHttpAnswer { correlation_id: b },
        )
        | (
            TimerType::ServiceAdmitAnswer { correlation_id: a, .. },
            TimerType::ServiceAdmitAnswer { correlation_id: b, .. },
        ) => a == b,
        (
            TimerType::FailureAnswer { change: a, .. },
            TimerType::FailureAnswer { change: b, .. },
        )
        | (TimerType::ReleaseAnswer { change: a }, TimerType::ReleaseAnswer { change: b }) => {
            a == b
        }
        _ => false,
    }
}

/// What an event is to the requests the call awaits answers to.
#[derive(Debug)]
pub enum Screened {
    /// Neither an answer nor a deadline: the rules read it.
    Other,
    /// The answer of a request whose deadline was pending: the deadline is
    /// cancelled, the rules read it.
    Awaited,
    /// A deadline expired: the rules read this event, the request's answer
    /// with nothing usable.
    Expired(CallEvent),
    /// An answer whose deadline expired, or an admit report under another
    /// limiter key: no rule reads it.
    Unawaited,
}

/// Screen `event` against the call's pending deadlines: a delivered deadline
/// fire (the timer screen already took it off the ledger) is read as its lost
/// answer; an answer takes the deadline it closes off `call`'s ledger and
/// cancels it on `fx`.
pub fn screen(call: &mut Call, fx: &mut HandlerEffects, event: &CallEvent) -> Screened {
    if let CallEvent::Timer { key, .. } = event {
        return match &key.timer_type {
            TimerType::ServiceHttpAnswer { correlation_id } => {
                Screened::Expired(lost_http_answer(call, correlation_id))
            }
            TimerType::ServiceAdmitAnswer { correlation_id, change } => {
                Screened::Expired(lost_admit_answer(call, correlation_id, *change))
            }
            TimerType::FailureAnswer { unanswered, .. } => {
                Screened::Expired(lost_failure_answer(call, unanswered))
            }
            _ => Screened::Other,
        };
    }
    let Some(answer) = answered(event).or_else(|| consult_answered(event)) else {
        return Screened::Other;
    };
    if of_another_call(call, event) {
        return Screened::Unawaited;
    }
    let Some(entry) = call::helpers::retire_timers(call, |t| closes(t.timer_type(), &answer)).pop()
    else {
        return Screened::Unawaited;
    };
    fx.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
    Screened::Awaited
}

/// Is `event` an admit report under another limiter key — an earlier call
/// under the same `call_ref`, whose correlation ids may repeat this call's?
fn of_another_call(call: &Call, event: &CallEvent) -> bool {
    crate::limiter::report::admit_report_of(event).is_some_and(|r| r.key != call.limiter.key())
}

/// The `error` result of the HTTP request `correlation_id` names.
fn lost_http_answer(call: &Call, correlation_id: &str) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: SERVICE_HTTP_RESULT.to_string(),
        outcome: "error".to_string(),
        payload: serde_json::json!({ "correlation_id": correlation_id, "error": NO_ANSWER }),
        body: Vec::new(),
        incarnation: Some(call.incarnation().to_string()),
    }
}

/// The `/call/failure` fold of a consult left unanswered: the stack's own
/// `503` ([`crate::failure_unavailable`]) with `unanswered`, the payload its
/// deadline carries.
fn lost_failure_answer(call: &Call, unanswered: &serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: FAILURE_TOPIC.to_string(),
        outcome: crate::failure_unavailable::OUTCOME.to_string(),
        payload: unanswered.clone(),
        body: Vec::new(),
        incarnation: Some(call.incarnation().to_string()),
    }
}

/// The `unavailable` result of the admit `correlation_id` names, numbered
/// `change`: its entries are the call's target while no later change replaced
/// it.
fn lost_admit_answer(call: &Call, correlation_id: &str, change: u64) -> CallEvent {
    LimiterAdmitResult {
        call_ref: call.call_ref.clone(),
        correlation_id: correlation_id.to_string(),
        report: call.limiter.lost_admit(change),
    }
    .into_event()
}

#[cfg(test)]
mod tests;
