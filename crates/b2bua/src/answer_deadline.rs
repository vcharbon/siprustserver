//! The deadline of the answer to a request sent off the call's turn: a
//! service's adaptation HTTP request (`RuleAction::ServiceHttpRequest`) or
//! replacement of the call's admission set (`RuleAction::ReplaceAdmissionSet`),
//! and the core's `/call/failure` consult (`RuleAction::FailureAsyncHttp`).
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
//!   deadline's expiry is the request's answer with nothing usable — an HTTP
//!   request's `error` result ([`NO_ANSWER`]), an admit's `unavailable`
//!   report, a failure consult's unanswered `terminate` fold — and one whose
//!   answer came first reaches none.

use std::time::Duration;

use call::{Call, CallModelState, TimerEntry, TimerType};

use crate::config::B2buaConfig;
use crate::decision_log::FAILURE_TOPIC;
use crate::effects::{CriticalStateEffect, FireAndForgetEffect, HandlerEffects, HandlerResult};
use crate::limiter::bounded::ADMIT_SLACK;
use crate::limiter::report::LimiterAdmitResult;
use b2bua_sdk::event::CallEvent;

/// How long past its budget a request's answer is still awaited: the answer's
/// way back through the node's queues on a healthy node.
pub const MARGIN: Duration = Duration::from_secs(2);

// The admit bound's slack lies well inside the margin.
const _: () = assert!(ADMIT_SLACK.as_millis() * 4 <= MARGIN.as_millis());

/// The `/call/failure` consults one failover chain may send: the first, and
/// one per limiter refusal it re-consults on.
pub const FAILURE_CHAIN: u32 = crate::decision::apply_route::MAX_LIMITER_FAILOVER + 1;

/// The budget a `/call/failure` consult is answered in, `decision` being the
/// decision engine's per-consult deadline and `admit` the limiter's admit
/// budget: each consult of the chain, then its route's admit (bounded just
/// past the admit budget). `None` when the decision deadline is off
/// (`decision` zero): the consult is unbounded and has no deadline.
pub fn failure_budget(decision: Duration, admit: Duration) -> Option<Duration> {
    (!decision.is_zero()).then(|| (decision + admit + ADMIT_SLACK) * FAILURE_CHAIN)
}

/// [`failure_budget`] under `config`'s decision deadline
/// (`call_control_timeout_ms`, off when not positive).
pub(crate) fn failure_budget_under(config: &B2buaConfig, admit: Duration) -> Option<Duration> {
    let decision = Duration::from_millis(config.call_control_timeout_ms.max(0) as u64);
    failure_budget(decision, admit)
}

/// The payload key a `/call/failure` fold states the deadline it answers
/// under (the consult's first change number), as [`arm`] named it on the
/// effect. A consult with no deadline states none, and its fold reaches the
/// rules unscreened.
pub(crate) const CONSULT_CHANGE: &str = "consult_change";

/// The `error` an HTTP request's result states when its deadline expired.
pub const NO_ANSWER: &str = "no_answer";

/// The topic of an adaptation HTTP request's result.
const SERVICE_HTTP_RESULT: &str = "service-http-result";

/// The budgets the router answers a service's requests by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnswerBudgets {
    /// An HTTP request that states no `timeout_ms` of its own.
    pub http: Duration,
    /// An admit.
    pub admit: Duration,
    /// A `/call/failure` consult's chain ([`failure_budget`]); `None` when
    /// the consult is unbounded.
    pub failure: Option<Duration>,
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
        if let FireAndForgetEffect::FailureAsyncHttp { limiter_change, deadline, .. } = fx {
            *deadline = budgets.failure.map(|_| *limiter_change);
        }
    }
    for (timer_type, budget) in deadlines {
        let entry = TimerEntry {
            id: timer_type.timer_id(None),
            timer_type,
            fire_at: now_ms + (budget + MARGIN).as_millis() as i64,
            leg_id: None,
        };
        result.call.timers = call::helpers::replace_timer_by_id(
            std::mem::take(&mut result.call.timers),
            entry.clone(),
        );
        result.effects.critical.push(CriticalStateEffect::ScheduleTimer(entry));
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
        FireAndForgetEffect::FailureAsyncHttp { request, limiter_change, .. } => {
            budgets.failure.map(|budget| {
                let unanswered = crate::failure_terminate::unanswered(request);
                (TimerType::FailureAnswer { change: *limiter_change, unanswered }, budget)
            })
        }
        FireAndForgetEffect::Reenter(event) => {
            answered(event).map(|timer_type| (timer_type, Duration::ZERO))
        }
        _ => None,
    }
}

/// The deadline a service request's answer closes: its result naming its
/// correlation id.
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

/// The id of the deadline a `/call/failure` fold closes: its consult's first
/// change number ([`CONSULT_CHANGE`]).
fn failure_answered(event: &CallEvent) -> Option<String> {
    let CallEvent::InternalEvent { topic, payload, .. } = event else {
        return None;
    };
    if topic != FAILURE_TOPIC {
        return None;
    }
    let change = payload.get(CONSULT_CHANGE)?.as_u64()?;
    // The id names the change alone.
    let unanswered = serde_json::Value::Null;
    Some(TimerType::FailureAnswer { change, unanswered }.timer_id(None))
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
    /// An answer whose deadline expired, an admit report under another
    /// limiter key, or a deadline whose answer came first: no rule reads it.
    Unawaited,
}

/// Screen `event` against the call's pending deadlines, taking the one it
/// closes off `call`'s ledger (and cancelling it on `fx` for an answer).
pub fn screen(call: &mut Call, fx: &mut HandlerEffects, event: &CallEvent) -> Screened {
    if let CallEvent::Timer { timer_type, .. } = event {
        let lost = match timer_type {
            TimerType::ServiceHttpAnswer { correlation_id } => {
                lost_http_answer(call, correlation_id)
            }
            TimerType::ServiceAdmitAnswer { correlation_id, change } => {
                lost_admit_answer(call, correlation_id, *change)
            }
            TimerType::FailureAnswer { unanswered, .. } => lost_failure_answer(call, unanswered),
            _ => return Screened::Other,
        };
        if !take(call, &timer_type.timer_id(None)) {
            return Screened::Unawaited;
        }
        return Screened::Expired(lost);
    }
    let Some(id) = answered(event)
        .map(|timer_type| timer_type.timer_id(None))
        .or_else(|| failure_answered(event))
    else {
        return Screened::Other;
    };
    if of_another_call(call, event) {
        return Screened::Unawaited;
    }
    if !take(call, &id) {
        return Screened::Unawaited;
    }
    fx.critical.push(CriticalStateEffect::CancelTimer { id });
    Screened::Awaited
}

/// Is `event` an admit report under another limiter key — an earlier call
/// under the same `call_ref`, whose correlation ids may repeat this call's?
fn of_another_call(call: &Call, event: &CallEvent) -> bool {
    crate::limiter::report::admit_report_of(event).is_some_and(|r| r.key != call.limiter.key())
}

/// Take the ledger entry `id` off `call`; whether it was there.
fn take(call: &mut Call, id: &str) -> bool {
    let before = call.timers.len();
    call.timers.retain(|t| t.id != id);
    call.timers.len() != before
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

/// The `/call/failure` fold of a consult left unanswered: `terminate` with
/// `unanswered`, the payload its deadline carries.
fn lost_failure_answer(call: &Call, unanswered: &serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: FAILURE_TOPIC.to_string(),
        outcome: crate::failure_terminate::OUTCOME.to_string(),
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
mod tests {
    use call::{AdmitOutcome, AdmitReport, CallLimiterState, LimiterEntry, LimiterHeld};

    use super::*;

    const BUDGETS: AnswerBudgets = AnswerBudgets {
        http: Duration::from_secs(3),
        admit: Duration::from_millis(150),
        failure: Some(Duration::from_secs(40)),
    };
    const NOW: i64 = 10_000;

    fn call() -> Call {
        use crate::router::test_support::{invite, src};
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let invite = invite("w0", "w1", "deadline");
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(&invite, src(), &config, &ids, 0);
        call.limiter = CallLimiterState::admitted("c#k".into(), 1, vec![entry("x")]);
        call
    }

    fn entry(id: &str) -> LimiterEntry {
        LimiterEntry { id: id.into(), limit: 10 }
    }

    fn http_request(correlation_id: &str, timeout_ms: Option<u64>) -> FireAndForgetEffect {
        FireAndForgetEffect::ServiceHttpRequest {
            call_ref: "c".into(),
            correlation_id: correlation_id.into(),
            endpoint: "/ask".into(),
            method: "POST".into(),
            headers: vec![],
            body: vec![],
            content_type: None,
            timeout_ms,
        }
    }

    fn admit(correlation_id: &str, change: u64) -> FireAndForgetEffect {
        FireAndForgetEffect::LimiterAdmit {
            call_ref: "c".into(),
            correlation_id: correlation_id.into(),
            key: "c#k".into(),
            change,
            held: LimiterHeld { change: 1, entries: vec![entry("x")] },
            entries: vec![entry("x"), entry("y")],
        }
    }

    fn sending(call: Call, effects: Vec<FireAndForgetEffect>) -> HandlerResult {
        let mut result = HandlerResult::new(call);
        result.effects.fire_and_forget = effects;
        result
    }

    fn http_result(correlation_id: &str) -> CallEvent {
        CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: SERVICE_HTTP_RESULT.into(),
            outcome: "ok".into(),
            payload: serde_json::json!({ "correlation_id": correlation_id, "status": 200 }),
            body: Vec::new(),
            incarnation: None,
        }
    }

    fn fired(timer_type: TimerType) -> CallEvent {
        CallEvent::Timer { timer_type, call_ref: "c".into(), leg_id: None, incarnation: None }
    }

    fn deadline(call: &Call, id: &str) -> Option<i64> {
        call.timers.iter().find(|t| t.id == id).map(|t| t.fire_at)
    }

    #[test]
    fn a_turn_sending_requests_arms_each_deadline_past_its_budget() {
        let result = arm(
            sending(
                call(),
                vec![http_request("a", None), http_request("b", Some(500)), admit("c", 7)],
            ),
            &BUDGETS,
            NOW,
        );
        let margin = MARGIN.as_millis() as i64;
        assert_eq!(deadline(&result.call, "ServiceHttpAnswer:a"), Some(NOW + 3_000 + margin));
        assert_eq!(deadline(&result.call, "ServiceHttpAnswer:b"), Some(NOW + 500 + margin));
        assert_eq!(deadline(&result.call, "ServiceAdmitAnswer:c"), Some(NOW + 150 + margin));
        let scheduled = result
            .effects
            .critical
            .iter()
            .filter(|e| matches!(e, CriticalStateEffect::ScheduleTimer(_)))
            .count();
        assert_eq!(scheduled, 3, "each deadline is scheduled on the driver");
    }

    #[test]
    fn a_turn_ending_the_call_arms_nothing() {
        let mut ended = call();
        ended.state = CallModelState::Terminated;
        let result = arm(sending(ended, vec![http_request("a", None)]), &BUDGETS, NOW);
        assert!(result.call.timers.is_empty() && result.effects.critical.is_empty());
    }

    #[test]
    fn an_answer_the_turn_re_enters_itself_is_awaited() {
        let not_sent = LimiterAdmitResult {
            call_ref: "c".into(),
            correlation_id: "c".into(),
            report: AdmitReport {
                key: "c#k".into(),
                change: 7,
                entries: vec![],
                outcome: AdmitOutcome::NotSent,
            },
        }
        .into_event();
        let result = arm(
            sending(call(), vec![FireAndForgetEffect::Reenter(Box::new(not_sent.clone()))]),
            &BUDGETS,
            NOW,
        );
        assert_eq!(
            deadline(&result.call, "ServiceAdmitAnswer:c"),
            Some(NOW + MARGIN.as_millis() as i64)
        );
        let mut call = result.call;
        let mut fx = HandlerEffects::new();
        assert!(matches!(screen(&mut call, &mut fx, &not_sent), Screened::Awaited));
    }

    #[test]
    fn an_awaited_answer_cancels_its_deadline_and_reaches_the_rules() {
        let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        assert!(matches!(screen(&mut call, &mut fx, &http_result("a")), Screened::Awaited));
        assert_eq!(deadline(&call, "ServiceHttpAnswer:a"), None, "off the ledger");
        assert!(matches!(
            fx.critical.as_slice(),
            [CriticalStateEffect::CancelTimer { id }] if id == "ServiceHttpAnswer:a"
        ));
    }

    #[test]
    fn an_answer_past_its_deadline_reaches_no_rule() {
        let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        let expiry = fired(TimerType::ServiceHttpAnswer { correlation_id: "a".into() });
        assert!(matches!(screen(&mut call, &mut fx, &expiry), Screened::Expired(_)));
        assert!(matches!(screen(&mut call, &mut fx, &http_result("a")), Screened::Unawaited));
        assert!(fx.critical.is_empty(), "nothing to cancel");
    }

    #[test]
    fn a_deadline_whose_answer_came_first_reaches_no_rule() {
        let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        assert!(matches!(screen(&mut call, &mut fx, &http_result("a")), Screened::Awaited));
        let expiry = fired(TimerType::ServiceHttpAnswer { correlation_id: "a".into() });
        assert!(matches!(screen(&mut call, &mut fx, &expiry), Screened::Unawaited));
    }

    #[test]
    fn an_expired_http_deadline_is_the_request_s_error_result() {
        let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        let expiry = fired(TimerType::ServiceHttpAnswer { correlation_id: "a".into() });
        let Screened::Expired(CallEvent::InternalEvent { topic, outcome, payload, body, .. }) =
            screen(&mut call, &mut fx, &expiry)
        else {
            panic!("an expired deadline is read as the request's answer");
        };
        assert_eq!((topic.as_str(), outcome.as_str()), (SERVICE_HTTP_RESULT, "error"));
        assert_eq!(payload, serde_json::json!({ "correlation_id": "a", "error": NO_ANSWER }));
        assert!(body.is_empty());
        assert_eq!(deadline(&call, "ServiceHttpAnswer:a"), None, "off the ledger");
    }

    #[test]
    fn an_expired_admit_deadline_is_an_unavailable_admit_of_the_asked_set() {
        let mut call = call();
        call.limiter = CallLimiterState::from_parts(
            call.limiter.key().to_string(),
            true,
            (vec![entry("x")], 1),
            (vec![entry("x"), entry("y")], 7),
            7,
            None,
        );
        let mut call = arm(sending(call, vec![admit("c", 7)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        let expiry = fired(TimerType::ServiceAdmitAnswer { correlation_id: "c".into(), change: 7 });
        let Screened::Expired(lost) = screen(&mut call, &mut fx, &expiry) else {
            panic!("an expired deadline is read as the admit's answer");
        };
        let CallEvent::InternalEvent { topic, outcome, payload, .. } = &lost else {
            panic!("an internal event");
        };
        assert_eq!((topic.as_str(), outcome.as_str()), (LimiterAdmitResult::TOPIC, "unavailable"));
        assert_eq!(payload["correlation_id"], "c");
        assert_eq!(
            crate::limiter::report::admit_report_of(&lost),
            Some(AdmitReport {
                key: "c#k".into(),
                change: 7,
                entries: vec![entry("x"), entry("y")],
                outcome: AdmitOutcome::Unavailable,
            })
        );
    }

    fn failure_consult(change: u64) -> FireAndForgetEffect {
        FireAndForgetEffect::FailureAsyncHttp {
            call_ref: "c".into(),
            request: serde_json::json!({
                "callback_context": "ctx",
                "origin": "external",
                "sip_code": 486,
                "sip_reason": "Busy Here",
                "failed_leg_id": "b-1",
                "sip_headers": [],
            }),
            limiter_change: change,
            deadline: None,
        }
    }

    fn failure_fold(change: Option<u64>) -> CallEvent {
        let mut payload = serde_json::json!({ "failed_leg_id": "b-1", "status": 486 });
        if let Some(change) = change {
            payload[CONSULT_CHANGE] = serde_json::json!(change);
        }
        CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: FAILURE_TOPIC.into(),
            outcome: "terminate".into(),
            payload,
            body: Vec::new(),
            incarnation: None,
        }
    }

    fn failure_deadline(change: u64) -> TimerType {
        TimerType::FailureAnswer { change, unanswered: serde_json::Value::Null }
    }

    #[test]
    fn a_failure_consult_is_answered_by_its_whole_chain() {
        let (decision, admit) = (Duration::from_secs(5), Duration::from_millis(150));
        assert_eq!(
            failure_budget(decision, admit),
            Some((decision + admit + ADMIT_SLACK) * FAILURE_CHAIN)
        );
        assert_eq!(FAILURE_CHAIN, 6, "the first consult and five re-consults");
        assert_eq!(failure_budget(Duration::ZERO, admit), None, "an unbounded consult");
    }

    #[test]
    fn a_failure_consult_arms_its_deadline_past_the_chain_budget() {
        let result = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW);
        assert_eq!(
            deadline(&result.call, "FailureAnswer:4"),
            Some(NOW + (Duration::from_secs(40) + MARGIN).as_millis() as i64)
        );
    }

    /// The deadline a failure consult's effect names, the fold to state.
    fn named(result: &HandlerResult) -> Option<u64> {
        match result.effects.fire_and_forget.as_slice() {
            [FireAndForgetEffect::FailureAsyncHttp { deadline, .. }] => *deadline,
            other => panic!("one failure consult: {other:?}"),
        }
    }

    #[test]
    fn a_failure_consult_names_the_deadline_armed_for_it() {
        let result = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW);
        assert_eq!(named(&result), Some(4));
    }

    #[test]
    fn an_unbounded_failure_consult_arms_no_deadline() {
        let budgets = AnswerBudgets { failure: None, ..BUDGETS };
        let result = arm(sending(call(), vec![failure_consult(4)]), &budgets, NOW);
        assert!(result.call.timers.is_empty() && result.effects.critical.is_empty());
        assert_eq!(named(&result), None, "its fold names no deadline");
    }

    #[test]
    fn an_expired_failure_deadline_is_the_consult_s_unanswered_terminate_fold() {
        let mut call = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        let entry = call.timers.iter().find(|t| t.id == "FailureAnswer:4").cloned().unwrap();
        let Screened::Expired(CallEvent::InternalEvent { topic, outcome, payload, .. }) =
            screen(&mut call, &mut fx, &fired(entry.timer_type))
        else {
            panic!("an expired deadline is read as the consult's answer");
        };
        assert_eq!((topic.as_str(), outcome.as_str()), (FAILURE_TOPIC, "terminate"));
        assert_eq!(
            payload,
            serde_json::json!({
                "status": 486,
                "reason": "Busy Here",
                "origin": "external",
                "failed_leg_id": "b-1",
                "stack_authored": true,
            }),
            "the fold an unanswered consult resolves to"
        );
        assert!(call.timers.is_empty(), "off the ledger");
    }

    #[test]
    fn a_failure_fold_naming_its_consult_cancels_the_deadline() {
        let mut call = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        assert!(matches!(screen(&mut call, &mut fx, &failure_fold(Some(4))), Screened::Awaited));
        assert!(matches!(
            fx.critical.as_slice(),
            [CriticalStateEffect::CancelTimer { id }] if id == "FailureAnswer:4"
        ));
        let expiry = fired(failure_deadline(4));
        assert!(matches!(screen(&mut call, &mut fx, &expiry), Screened::Unawaited));
        assert!(matches!(screen(&mut call, &mut fx, &failure_fold(Some(4))), Screened::Unawaited));
    }

    #[test]
    fn a_failure_fold_naming_no_consult_is_left_to_the_rules() {
        let mut call = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        assert!(matches!(screen(&mut call, &mut fx, &failure_fold(None)), Screened::Other));
        assert!(deadline(&call, "FailureAnswer:4").is_some(), "still awaited");
    }

    #[test]
    fn an_event_that_is_no_request_s_answer_is_left_to_the_rules() {
        let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
        let mut fx = HandlerEffects::new();
        let keepalive = fired(TimerType::Keepalive);
        assert!(matches!(screen(&mut call, &mut fx, &keepalive), Screened::Other));
        let uncorrelated = CallEvent::InternalEvent {
            call_ref: "c".into(),
            topic: SERVICE_HTTP_RESULT.into(),
            outcome: "ok".into(),
            payload: serde_json::json!({}),
            body: Vec::new(),
            incarnation: None,
        };
        assert!(matches!(screen(&mut call, &mut fx, &uncorrelated), Screened::Other));
        assert!(deadline(&call, "ServiceHttpAnswer:a").is_some(), "still awaited");
    }
}
