use call::{AdmitOutcome, AdmitReport, CallLimiterState, LimiterEntry, LimiterHeld};

use super::*;

const BUDGETS: AnswerBudgets = AnswerBudgets {
    http: Duration::from_secs(3),
    admit: Duration::from_millis(150),
    failure: Some(Duration::from_secs(40)),
    release: Some(Duration::from_secs(8)),
};
const NOW: i64 = 10_000;

fn call() -> Call {
    use crate::test_call::{invite, src};
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = invite("w0", "w1", "deadline");
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call = crate::initial_invite::build_initial_call(&invite, src(), &config, &mut ids, 0);
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
    CallEvent::Timer {
        key: TimerKey::call(timer_type),
        generation: 1,
        call_ref: "c".into(),
        incarnation: None,
    }
}

/// The armed deadline the ledger names `id`, as its `Debug` states it.
fn armed(call: &Call, id: &str) -> Option<call::TimerEntry> {
    call.timers.iter().find(|t| format!("{:?}", t.timer_type()) == id).cloned()
}

fn deadline(call: &Call, id: &str) -> Option<i64> {
    armed(call, id).map(|t| t.fire_at)
}

/// The fire of the deadline the ledger names `id` (generation 0 when none is
/// armed), as the router delivers it: through the timer screen, which drops a
/// fire the ledger no longer holds, then through [`screen`].
fn deliver(
    call: &mut Call,
    fx: &mut HandlerEffects,
    id: &str,
    timer_type: TimerType,
) -> Option<Screened> {
    let generation = armed(call, id).map_or(0, |t| t.generation);
    let key = armed(call, id).map_or(TimerKey::call(timer_type), |t| t.key);
    let event = CallEvent::Timer { key, generation, call_ref: "c".into(), incarnation: None };
    crate::rule_chain::timer_screen::screen(call, &event, NOW, &mut Default::default()).ok()?;
    Some(screen(call, fx, &event))
}

#[test]
fn a_turn_sending_requests_arms_each_deadline_past_its_budget() {
    let result = arm(
        sending(call(), vec![http_request("a", None), http_request("b", Some(500)), admit("c", 7)]),
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
        [CriticalStateEffect::CancelTimer { key }]
            if format!("{:?}", key.timer_type) == "ServiceHttpAnswer:a"
    ));
}

#[test]
fn an_answer_past_its_deadline_reaches_no_rule() {
    let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    let http = TimerType::ServiceHttpAnswer { correlation_id: "a".into() };
    let expired = deliver(&mut call, &mut fx, "ServiceHttpAnswer:a", http);
    assert!(matches!(expired, Some(Screened::Expired(_))));
    assert!(matches!(screen(&mut call, &mut fx, &http_result("a")), Screened::Unawaited));
    assert!(fx.critical.is_empty(), "nothing to cancel");
}

#[test]
fn a_deadline_whose_answer_came_first_reaches_no_rule() {
    let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    assert!(matches!(screen(&mut call, &mut fx, &http_result("a")), Screened::Awaited));
    let http = TimerType::ServiceHttpAnswer { correlation_id: "a".into() };
    assert!(
        deliver(&mut call, &mut fx, "ServiceHttpAnswer:a", http).is_none(),
        "the timer screen drops the fire of a deadline off the ledger"
    );
}

#[test]
fn an_expired_http_deadline_is_the_request_s_error_result() {
    let mut call = arm(sending(call(), vec![http_request("a", None)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    let http = TimerType::ServiceHttpAnswer { correlation_id: "a".into() };
    let Some(Screened::Expired(CallEvent::InternalEvent { topic, outcome, payload, body, .. })) =
        deliver(&mut call, &mut fx, "ServiceHttpAnswer:a", http)
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
fn an_expired_failure_deadline_is_the_consult_s_unanswered_503_fold() {
    let mut call = arm(sending(call(), vec![failure_consult(4)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    let Some(Screened::Expired(CallEvent::InternalEvent { topic, outcome, payload, .. })) =
        deliver(&mut call, &mut fx, "FailureAnswer:4", failure_deadline(4))
    else {
        panic!("an expired deadline is read as the consult's answer");
    };
    assert_eq!((topic.as_str(), outcome.as_str()), (FAILURE_TOPIC, "reject"));
    assert_eq!(
        payload,
        serde_json::json!({
            "code": 503,
            "origin": "decision_unavailable",
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
        [CriticalStateEffect::CancelTimer { key }]
            if format!("{:?}", key.timer_type) == "FailureAnswer:4"
    ));
    assert!(
        deliver(&mut call, &mut fx, "FailureAnswer:4", failure_deadline(4)).is_none(),
        "the timer screen drops the fire of a deadline off the ledger"
    );
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

fn release_consult(change: u64) -> FireAndForgetEffect {
    FireAndForgetEffect::ReleaseAsyncHttp {
        call_ref: "c".into(),
        request: serde_json::json!({ "callback_context": "ctx", "event": "max_call_duration" }),
        limiter_change: change,
        deadline: None,
    }
}

fn release_fold(change: Option<u64>) -> CallEvent {
    let mut payload = serde_json::json!({ "destination": { "host": "127.0.0.1", "port": 5090 } });
    if let Some(change) = change {
        payload[CONSULT_CHANGE] = serde_json::json!(change);
    }
    CallEvent::InternalEvent {
        call_ref: "c".into(),
        topic: RELEASE_TOPIC.into(),
        outcome: "reroute".into(),
        payload,
        body: Vec::new(),
        incarnation: None,
    }
}

#[test]
fn a_release_consult_is_answered_by_its_decision_and_its_route_s_admit() {
    let (decision, admit) = (Duration::from_secs(5), Duration::from_millis(2_650));
    assert_eq!(release_budget(decision, admit), Some(decision + admit + ADMIT_SLACK));
    assert_eq!(release_budget(Duration::ZERO, admit), None, "an unbounded consult");
}

#[test]
fn a_release_consult_arms_its_deadline_and_names_it() {
    let result = arm(sending(call(), vec![release_consult(7)]), &BUDGETS, NOW);
    assert_eq!(
        deadline(&result.call, "ReleaseAnswer:7"),
        Some(NOW + (Duration::from_secs(8) + MARGIN).as_millis() as i64)
    );
    assert!(matches!(
        result.effects.fire_and_forget.as_slice(),
        [FireAndForgetEffect::ReleaseAsyncHttp { deadline: Some(7), .. }]
    ));
}

#[test]
fn a_release_fold_is_answered_once() {
    let mut call = arm(sending(call(), vec![release_consult(7)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    assert!(matches!(screen(&mut call, &mut fx, &release_fold(Some(7))), Screened::Awaited));
    assert!(deadline(&call, "ReleaseAnswer:7").is_none(), "the answer took its deadline");
    assert!(
        matches!(screen(&mut call, &mut fx, &release_fold(Some(7))), Screened::Unawaited),
        "a second answer of the consult reaches no rule"
    );
}

#[test]
fn an_expired_release_deadline_is_the_release_event_for_the_rules() {
    let mut call = arm(sending(call(), vec![release_consult(7)]), &BUDGETS, NOW).call;
    let mut fx = HandlerEffects::new();
    assert!(matches!(
        deliver(&mut call, &mut fx, "ReleaseAnswer:7", TimerType::ReleaseAnswer { change: 7 }),
        Some(Screened::Other)
    ));
    assert!(
        matches!(screen(&mut call, &mut fx, &release_fold(Some(7))), Screened::Unawaited),
        "the answer landing after its deadline reaches no rule"
    );
}
