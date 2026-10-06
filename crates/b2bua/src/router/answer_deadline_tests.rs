//! The router's turn on the answer to a request sent off the call's turn and
//! on its deadline (ADR-0039), driven through [`rule_chain_turn`] over a wired
//! core running a probe service that records, on the call, the outcome of
//! every service answer its rules read; a `/call/failure` fold reaches the
//! core's failover rules.

use call::{
    AdmitOutcome, AdmitReport, Call, CallLimiterState, Direction, LimiterEntry, StateLabel,
    TimerEntry, TimerType,
};

use super::process::rule_chain_turn;
use super::resolve::Resolution;
use super::test_support::{invite, node_serving, src, Node};
use crate::config::B2buaConfig;
use crate::effects::CriticalStateEffect;
use crate::initial_invite::build_initial_call;
use crate::limiter::report::LimiterAdmitResult;
use b2bua_sdk::event::CallEvent;

mod probe {
    use b2bua_sdk::{define_service, sm_rule};
    use call::ExtMap;

    use crate::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
    };
    use b2bua_sdk::event::CallEvent;

    define_service! {
        id: "probe",
        machine: PROBE,
        states: ProbeState { Awaiting },
        init: |_call: &RuleCall| None,
        rules: [ http(), admit() ],
    }

    /// The outcome of the internal event, recorded in the call's `probe` slice.
    fn record(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let CallEvent::InternalEvent { outcome, .. } = ctx.event else { return None };
        let mut ext = ExtMap::new();
        ext.insert("probe".into(), serde_json::json!(outcome));
        Some(RuleHandleResult::new(vec![RuleAction::MergeCallExt { ext }]))
    }

    fn http() -> RuleDefinition {
        sm_rule! {
            id: "probe-http",
            machine: PROBE,
            active: [ ProbeState::Awaiting ],
            transitions: [],
            effects: [],
            matcher: Match::internal_event().topic("service-http-result"),
            handle: record,
        }
    }

    fn admit() -> RuleDefinition {
        sm_rule! {
            id: "probe-admit",
            machine: PROBE,
            active: [ ProbeState::Awaiting ],
            transitions: [],
            effects: [],
            matcher: Match::internal_event().topic("limiter-admit-result"),
            handle: record,
        }
    }
}

const NOW: i64 = 50_000;

async fn wired() -> Node {
    node_serving("w0", |_| {}, vec![probe::service_def()]).await
}

/// A call awaiting the probe's answers, holding `[x]` under `c#k`.
fn awaiting_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "deadline-turn"),
        src(),
        &config,
        &sip_txn::IdGen::seeded(1),
        0,
    );
    call.sm_cursors.insert(probe::PROBE, StateLabel::new("Awaiting"));
    call.limiter = CallLimiterState::admitted("c#k".into(), 1, vec![entry("x")]);
    call
}

fn entry(id: &str) -> LimiterEntry {
    LimiterEntry { id: id.into(), limit: 10 }
}

fn awaiting(mut call: Call, timer_type: TimerType) -> Call {
    call.timers.push(TimerEntry {
        id: timer_type.timer_id(None),
        timer_type,
        fire_at: NOW - 1,
        leg_id: None,
    });
    call
}

fn http_deadline() -> TimerType {
    TimerType::ServiceHttpAnswer { correlation_id: "probe:1".into() }
}

fn admit_deadline() -> TimerType {
    TimerType::ServiceAdmitAnswer { correlation_id: "probe:2".into(), change: 2 }
}

fn http_answer(call: &Call) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: "service-http-result".into(),
        outcome: "ok".into(),
        payload: serde_json::json!({ "correlation_id": "probe:1", "status": 200 }),
        body: Vec::new(),
        incarnation: None,
    }
}

fn admit_answer(call: &Call, key: &str) -> CallEvent {
    LimiterAdmitResult {
        call_ref: call.call_ref.clone(),
        correlation_id: "probe:2".into(),
        report: AdmitReport {
            key: key.into(),
            change: 2,
            entries: vec![entry("x"), entry("y")],
            outcome: AdmitOutcome::Admitted,
        },
    }
    .into_event()
}

fn fired(call: &Call, timer_type: TimerType) -> CallEvent {
    CallEvent::Timer {
        timer_type,
        call_ref: call.call_ref.clone(),
        leg_id: None,
        incarnation: None,
    }
}

/// One turn of `call` on `event`: the handler result (the call it leaves and
/// the effects).
fn turn(node: &Node, call: Call, event: &CallEvent) -> crate::effects::HandlerResult {
    let res = Resolution {
        call_ref: Some(call.call_ref.clone()),
        source_leg_id: call.a_leg.leg_id.clone(),
        direction: Direction::FromA,
        initial_invite: false,
        incarnation: None,
        indexed: None,
    };
    let call_ref = call.call_ref.clone();
    rule_chain_turn(node.core.router_ctx(), call, event, &res, &call_ref, NOW)
}

/// The outcome the probe's rules read, if any rule read an answer.
fn read(call: &Call) -> Option<String> {
    call.ext.as_ref()?.get("probe")?.as_str().map(str::to_string)
}

#[tokio::test(start_paused = true)]
async fn an_awaited_answer_reaches_the_rules_and_cancels_its_deadline() {
    let node = wired().await;
    let call = awaiting(awaiting_call(), http_deadline());
    let event = http_answer(&call);
    let result = turn(&node, call, &event);
    assert_eq!(read(&result.call).as_deref(), Some("ok"));
    assert!(result.call.timers.is_empty(), "the deadline is off the ledger");
    assert!(result.effects.critical.iter().any(|e| matches!(
        e,
        CriticalStateEffect::CancelTimer { id } if id == "ServiceHttpAnswer:probe:1"
    )));
}

#[tokio::test(start_paused = true)]
async fn an_http_answer_past_its_deadline_reaches_no_rule() {
    let node = wired().await;
    let call = awaiting_call();
    let event = http_answer(&call);
    let result = turn(&node, call, &event);
    assert_eq!(read(&result.call), None, "no rule reads a late answer");
}

#[tokio::test(start_paused = true)]
async fn an_admit_answer_past_its_deadline_reaches_no_rule_and_states_the_held_set() {
    let node = wired().await;
    let call = awaiting_call();
    let event = admit_answer(&call, "c#k");
    let result = turn(&node, call, &event);
    assert_eq!(read(&result.call), None, "no rule reads a late answer");
    assert_eq!(
        result.call.limiter.held(),
        [entry("x"), entry("y")],
        "the report still states what the limiter holds"
    );
}

#[tokio::test(start_paused = true)]
async fn an_expired_deadline_reaches_the_rules_as_the_lost_answer() {
    let node = wired().await;
    let call = awaiting(awaiting_call(), http_deadline());
    let event = fired(&call, http_deadline());
    let result = turn(&node, call, &event);
    assert_eq!(read(&result.call).as_deref(), Some("error"));
    assert!(result.call.timers.is_empty(), "the deadline is off the ledger");
}

#[tokio::test(start_paused = true)]
async fn a_deadline_whose_answer_came_first_reaches_no_rule() {
    let node = wired().await;
    let call = awaiting_call();
    let event = fired(&call, admit_deadline());
    let result = turn(&node, call, &event);
    assert_eq!(read(&result.call), None);
}

/// A report of an earlier call under the same `call_ref` (another limiter
/// key) is no answer of this call's: its deadline stays, no rule reads it,
/// and this call's own answer is still awaited.
#[tokio::test(start_paused = true)]
async fn an_admit_answer_of_another_key_leaves_the_call_s_deadline_awaited() {
    let node = wired().await;
    let call = awaiting(awaiting_call(), admit_deadline());
    let stale = admit_answer(&call, "c#earlier");
    let result = turn(&node, call, &stale);
    assert_eq!(read(&result.call), None, "no rule reads another call's report");
    assert_eq!(result.call.timers.len(), 1, "the call's deadline is still awaited");
    let own = admit_answer(&result.call, "c#k");
    let result = turn(&node, result.call, &own);
    assert_eq!(read(&result.call).as_deref(), Some("admitted"));
    assert!(result.call.timers.is_empty());
}

// ── a `/call/failure` consult's answer ──────────────────────────────────────

/// The deadline of the `/call/failure` consult numbered `change`, raised by
/// the callee's 486 on `b-1`.
fn failure_deadline(change: u64) -> TimerType {
    let request = serde_json::json!({
        "origin": "external",
        "sip_code": 486,
        "sip_reason": "Busy Here",
        "failed_leg_id": "b-1",
    });
    TimerType::FailureAnswer { change, unanswered: crate::failure_terminate::unanswered(&request) }
}

/// The consult `change`'s fold `outcome`, carrying the admit its task sent
/// (`[x, y]` admitted under the call's key).
fn failure_answer(call: &Call, change: u64, outcome: &str) -> CallEvent {
    let report = AdmitReport {
        key: "c#k".into(),
        change,
        entries: vec![entry("x"), entry("y")],
        outcome: AdmitOutcome::Admitted,
    };
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: "call-failure-result".into(),
        outcome: outcome.into(),
        payload: serde_json::json!({
            "destination": { "host": "127.0.0.1", "port": 5090 },
            "failed_leg_id": "b-1",
            "status": 486,
            "reason": "Busy Here",
            "subscriptions": [],
            "features": {},
            crate::answer_deadline::CONSULT_CHANGE: change,
            LimiterAdmitResult::REPORT: report,
        }),
        body: Vec::new(),
        incarnation: None,
    }
}

fn ending(call: &Call) -> bool {
    matches!(call.state, call::CallModelState::Terminating | call::CallModelState::Terminated)
}

#[tokio::test(start_paused = true)]
async fn an_expired_failure_deadline_relays_the_failed_final_and_ends_the_call() {
    let node = wired().await;
    let call = awaiting(awaiting_call(), failure_deadline(4));
    let event = fired(&call, failure_deadline(4));
    let result = turn(&node, call, &event);
    assert!(ending(&result.call), "the unanswered consult ends the call");
    let termination = result.call.termination.as_ref().expect("a termination record");
    assert_eq!(termination.cause, call::TerminationCause::RemoteFinal);
    assert!(result.call.timers.iter().all(|t| t.id != "FailureAnswer:4"), "off the ledger");
}

#[tokio::test(start_paused = true)]
async fn a_failure_answer_past_its_deadline_reaches_no_rule_and_states_the_held_set() {
    let node = wired().await;
    let call = awaiting_call();
    let legs = call.b_legs.len();
    let event = failure_answer(&call, 4, "failover");
    let result = turn(&node, call, &event);
    assert_eq!(result.call.b_legs.len(), legs, "no rule dials the late failover route");
    assert!(!ending(&result.call));
    assert_eq!(
        result.call.limiter.held(),
        [entry("x"), entry("y")],
        "the report still states what the limiter holds"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_deadline_whose_answer_came_first_reaches_no_rule() {
    let node = wired().await;
    let call = awaiting_call();
    let event = fired(&call, failure_deadline(4));
    let result = turn(&node, call, &event);
    assert!(!ending(&result.call), "a spent deadline ends nothing");
}

#[tokio::test(start_paused = true)]
async fn an_awaited_failure_answer_reaches_the_rules_and_cancels_its_deadline() {
    let node = wired().await;
    let call = awaiting(awaiting_call(), failure_deadline(4));
    let event = failure_answer(&call, 4, "terminate");
    let result = turn(&node, call, &event);
    assert!(ending(&result.call), "the terminate fold ends the call");
    assert!(result.effects.critical.iter().any(|e| matches!(
        e,
        CriticalStateEffect::CancelTimer { id } if id == "FailureAnswer:4"
    )));
}
