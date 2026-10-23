//! Lifecycle delivery: every active service hears the turn's lifecycle
//! events, whichever rule claimed the turn, within what a lifecycle handler
//! may do; and a guard timer is declared by its type and its scope's kind.

use super::*;
use b2bua_core::rules::lifecycle_delivery::{close_turn, TurnStart};
use b2bua_sdk::model::LifecycleEvent;

fn bye_event() -> CallEvent {
    CallEvent::sip(
        SipMessage::Request(in_dialog_request(sip_message::Method::Bye)),
        "127.0.0.1:5060".parse().unwrap(),
    )
}

fn ext_write(key: &str, value: serde_json::Value) -> RuleAction {
    RuleAction::MergeCallExt { ext: [(key.to_string(), value)].into_iter().collect() }
}

/// The listening machine's own guard timer.
const WATCH: TimerType = TimerType::service(MachineId::new(TEST_MACHINE), "watch");
static WATCH_EFFECTS: [Effect; 1] =
    [Effect::GuardTimer { timer: WATCH, scope: call::TimerScopeKind::Call, label: "arm watch" }];

/// Hears a leg end on its own machine: arms its guard timer, moves its cursor
/// S0 → S1 and writes its slice, naming the leg.
fn on_leg_ended_arming(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let Some(LifecycleEvent::LegEnded { leg, .. }) = ctx.lifecycle() else {
        return None;
    };
    Some(RuleHandleResult::new(vec![
        RuleAction::arm(TimerKey::call(WATCH), TimerDelay::secs(5)),
        RuleAction::SetState { machine: MachineId::new(TEST_MACHINE), to: StateLabel::new("S1") },
        ext_write("seen", serde_json::json!({ "ended": leg })),
    ]))
}

/// Hears a leg end and answers a final of its own.
fn on_leg_ended_answering(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::Respond {
        status: 486,
        reason: "Busy Here".into(),
        body: vec![],
        content_type: None,
    }]))
}

/// Hears a leg end and arms a core timer, which is no machine's own.
fn on_leg_ended_arming_a_core_timer(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::arm(
        TimerKey::call(TimerType::NoAnswer),
        TimerDelay::secs(5),
    )]))
}

/// Hears a leg end and arms another machine's guard timer.
fn on_leg_ended_arming_a_foreign_timer(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::arm(
        TimerKey::call(TimerType::service(MachineId::new("other-machine"), "watch")),
        TimerDelay::secs(5),
    )]))
}

/// Hears a leg end and moves its cursor along an edge it did not declare.
fn on_leg_ended_undeclared_move(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(TEST_MACHINE),
        to: StateLabel::new("S2"),
    }]))
}

/// A machine-bound leg-end listener active in S0, declaring S0 → S1 and its
/// timer.
fn leg_end_listener(handle: fn(&RuleContext) -> Option<RuleHandleResult>) -> RuleDefinition {
    RuleDefinition {
        id: "test-leg-end-listener",
        matcher: Match::leg_ended(),
        effects: &WATCH_EFFECTS,
        ..sm_rule(handle)
    }
}

/// A rule that claims the BYE, answers it and ends the caller's leg: the
/// call itself stays up.
fn answering_claimer() -> RuleDefinition {
    RuleDefinition::core(
        "test-answering-claimer",
        b2bua_core::rules::CORE_LAYER,
        &[],
        Match::request().method("BYE"),
        |_| {
            Some(RuleHandleResult::new(vec![
                RuleAction::Respond {
                    status: 200,
                    reason: "OK".into(),
                    body: vec![],
                    content_type: None,
                },
                RuleAction::TerminateLeg { leg_id: "a".into(), end: LegEnd::PeerByeAnswered },
            ]))
        },
    )
}

/// The BYE turn under `rules`, closed: its lifecycle settled and delivered.
fn run_bye(rules: &[RuleDefinition], call: &call::Call) -> HandlerResult {
    run_bye_noting(rules, call).0
}

/// [`run_bye`], with the details of the defects the turn noted.
fn run_bye_noting(rules: &[RuleDefinition], call: &call::Call) -> (HandlerResult, Vec<String>) {
    let event = bye_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let ctx = ctx_for(call, &event, &config);
    let claimed = execute_rules(rules, call, &ctx, &mut exec, &ObligationSet::core());
    let result = close_turn(rules, &TurnStart::of(call), claimed, &mut exec);
    (result, defects(&exec.turn.notes))
}

/// The details of the defects `notes` carry.
fn defects(notes: &b2bua_core::turn_notes::TurnNotes) -> Vec<String> {
    notes.defects().iter().map(|d| d.detail.clone()).collect()
}

fn confirmed_call_in_s0() -> call::Call {
    let mut call = test_call();
    confirm_a(&mut call);
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    call
}

fn watch_scheduled(result: &HandlerResult) -> bool {
    result
        .effects
        .critical
        .iter()
        .any(|e| matches!(e, CriticalStateEffect::ScheduleTimer(t) if *t.timer_type() == WATCH))
}

/// The leg a turn ended reaches a service that did not claim the turn: its
/// handler arms its own timer, moves its own cursor and writes its slice,
/// beside the claimer's handling.
#[test]
fn a_leg_end_reaches_a_service_that_did_not_claim_the_turn() {
    let rules = vec![leg_end_listener(on_leg_ended_arming), answering_claimer()];
    let result = run_bye(&rules, &confirmed_call_in_s0());
    assert_eq!(finals_to_a(&result), vec![200], "the claimer answered the BYE");
    assert_eq!(
        result.call.ext.as_ref().and_then(|e| e.get("seen")),
        Some(&serde_json::json!({ "ended": "a" })),
        "the listener heard the caller's leg end",
    );
    assert!(
        result.call.timers.iter().any(|t| *t.timer_type() == WATCH) && watch_scheduled(&result),
        "its timer is armed: {:?}",
        result.call.timers,
    );
    assert_eq!(
        result.call.sm_cursors.get(&MachineId::new(TEST_MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "it moved its own cursor",
    );
}

/// Without a leg end the listener hears nothing: the same claim, no write.
#[test]
fn a_turn_ending_no_leg_delivers_no_leg_end() {
    let answer_only = RuleDefinition::core(
        "test-answer-only",
        b2bua_core::rules::CORE_LAYER,
        &[],
        Match::request().method("BYE"),
        |_| {
            Some(RuleHandleResult::new(vec![RuleAction::Respond {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
            }]))
        },
    );
    let rules = vec![leg_end_listener(on_leg_ended_arming), answer_only];
    let result = run_bye(&rules, &confirmed_call_in_s0());
    assert!(result.call.ext.as_ref().is_none_or(|e| e.get("seen").is_none()));
    assert!(!watch_scheduled(&result));
}

/// A service the turn's teardown deactivated still hears the leg end the
/// turn caused, for its call-ext writes alone: its timer and its cursor go
/// with the machine.
#[test]
fn a_service_the_turn_deactivated_hears_the_leg_end_for_its_ext_writes() {
    let mut rules = vec![leg_end_listener(on_leg_ended_arming)];
    rules.extend(default_rules());
    let result = run_bye(&rules, &confirmed_call_in_s0());
    assert_ne!(result.call.state, CallModelState::Active, "relay-bye tore the call down");
    assert_eq!(
        result.call.ext.as_ref().and_then(|e| e.get("seen")),
        Some(&serde_json::json!({ "ended": "a" })),
    );
    assert!(!watch_scheduled(&result), "no timer armed for a deactivated machine");
    assert!(result.call.timers.iter().all(|t| *t.timer_type() != WATCH));
    assert!(!result.call.sm_cursors.contains_key(&MachineId::new(TEST_MACHINE)));
}

/// A turn that terminates the call outright (no termination window) still
/// delivers the leg end it caused, but a handler arms nothing on the
/// terminated call: its record is removed as the turn lands.
#[test]
fn a_handler_arms_nothing_on_a_terminated_call() {
    let terminating = RuleDefinition::core(
        "test-terminating-claimer",
        b2bua_core::rules::CORE_LAYER,
        &[],
        Match::request().method("BYE"),
        |_| {
            Some(RuleHandleResult::new(vec![
                RuleAction::Respond {
                    status: 200,
                    reason: "OK".into(),
                    body: vec![],
                    content_type: None,
                },
                RuleAction::TerminateLeg { leg_id: "a".into(), end: LegEnd::PeerByeAnswered },
                RuleAction::TerminateCall { cause: TerminationCause::RemoteBye, by_leg: None },
            ]))
        },
    );
    let rules = vec![leg_end_listener(on_leg_ended_arming), terminating];
    let result = run_bye(&rules, &confirmed_call_in_s0());
    assert_eq!(result.call.state, CallModelState::Terminated);
    assert_eq!(
        result.call.ext.as_ref().and_then(|e| e.get("seen")),
        Some(&serde_json::json!({ "ended": "a" })),
        "the listener still heard the leg end",
    );
    assert!(!watch_scheduled(&result), "no timer scheduled on a terminated call");
    assert!(result.call.timers.iter().all(|t| *t.timer_type() != WATCH));
}

/// A machine-less rule is no service: it hears no lifecycle event.
#[test]
fn a_core_rule_hears_no_lifecycle_event() {
    let core_listener =
        RuleDefinition::core("test-core-listener", SERVICE_LAYER, &[], Match::leg_ended(), |_| {
            Some(RuleHandleResult::new(vec![ext_write("seen", serde_json::json!(true))]))
        });
    let rules = vec![core_listener, answering_claimer()];
    let result = run_bye(&rules, &confirmed_call_in_s0());
    assert!(result.call.ext.as_ref().is_none_or(|e| e.get("seen").is_none()));
}

/// Whether one of `defects` says `wanted`.
fn noted(defects: &[String], wanted: &str) -> bool {
    defects.iter().any(|d| d.contains(wanted))
}

/// What a lifecycle handler may write, as its defect states it.
const OUT_OF_SCOPE: &str =
    "a lifecycle handler writes call ext, its own machine's cursor and its own timers only";

/// A lifecycle handler writes call ext, its own cursor and its own timers: a
/// final in one is an authoring bug, noted as the turn's defect.
#[test]
fn a_lifecycle_handler_stating_a_final_is_an_authoring_bug() {
    let rules = vec![leg_end_listener(on_leg_ended_answering), answering_claimer()];
    let (_, defects) = run_bye_noting(&rules, &confirmed_call_in_s0());
    assert!(noted(&defects, OUT_OF_SCOPE), "{defects:?}");
}

/// A core timer is no machine's own.
#[test]
fn a_lifecycle_handler_arming_a_core_timer_is_an_authoring_bug() {
    let rules = vec![leg_end_listener(on_leg_ended_arming_a_core_timer), answering_claimer()];
    let (_, defects) = run_bye_noting(&rules, &confirmed_call_in_s0());
    assert!(noted(&defects, OUT_OF_SCOPE), "{defects:?}");
}

/// Another machine's timer is not the handler's either.
#[test]
fn a_lifecycle_handler_arming_a_foreign_timer_is_an_authoring_bug() {
    let rules = vec![leg_end_listener(on_leg_ended_arming_a_foreign_timer), answering_claimer()];
    let (_, defects) = run_bye_noting(&rules, &confirmed_call_in_s0());
    assert!(noted(&defects, OUT_OF_SCOPE), "{defects:?}");
}

/// A lifecycle handler's cursor move is held to its declared edges.
#[test]
fn a_lifecycle_handlers_undeclared_move_is_an_authoring_bug() {
    let rules = vec![leg_end_listener(on_leg_ended_undeclared_move), answering_claimer()];
    let (_, defects) = run_bye_noting(&rules, &confirmed_call_in_s0());
    assert!(noted(&defects, "caused an undeclared transition"), "{defects:?}");
}

/// The actions a lifecycle handler may not take are dropped: the turn is the
/// claimer's and the handler's admitted writes alone.
#[test]
fn a_lifecycle_handler_leaks_nothing_it_may_not_take() {
    let call = confirmed_call_in_s0();
    let bare = run_bye(&[answering_claimer()], &call);
    let handles: [fn(&RuleContext) -> Option<RuleHandleResult>; 3] = [
        on_leg_ended_answering,
        on_leg_ended_arming_a_core_timer,
        on_leg_ended_arming_a_foreign_timer,
    ];
    for handle in handles {
        let rules = vec![leg_end_listener(handle), answering_claimer()];
        let result = run_bye(&rules, &call);
        assert_eq!(finals_to_a(&result), vec![200], "no 486 from the handler");
        assert_eq!(result.call, bare.call, "nothing of the handler's lands");
    }
}

/// Runs `handle` on an INFO as the machine's rule declaring `WATCH` on the
/// call's scope.
fn run_watching(handle: fn(&RuleContext) -> Option<RuleHandleResult>) -> HandlerResult {
    run_watching_noting(handle).0
}

/// [`run_watching`], with the details of the defects the turn noted.
fn run_watching_noting(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> (HandlerResult, Vec<String>) {
    let call = confirmed_call_in_s0();
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![RuleDefinition { effects: &WATCH_EFFECTS, ..sm_rule(handle) }];
    let ctx = ctx_for(&call, &event, &config);
    let result = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    (result, defects(&exec.turn.notes))
}

/// A timer armed under the scope its declaration names is declared.
#[test]
fn a_timer_armed_under_its_declared_scope_is_declared() {
    let result = run_watching(|_| {
        Some(RuleHandleResult::new(vec![RuleAction::arm(
            TimerKey::call(WATCH),
            TimerDelay::secs(5),
        )]))
    });
    assert!(watch_scheduled(&result));
}

/// The declaration names the timer's type and the kind of scope its key
/// guards: the same type under another scope is an undeclared timer.
#[test]
fn a_timer_armed_under_another_scope_than_declared_is_an_authoring_bug() {
    let (_, defects) = run_watching_noting(|_| {
        Some(RuleHandleResult::new(vec![RuleAction::arm(
            TimerKey::leg(WATCH, "a"),
            TimerDelay::secs(5),
        )]))
    });
    assert!(noted(&defects, "emitted an undeclared timer"), "{defects:?}");
}

/// A service timer is declared by its owning service and its key: another
/// key of the same service is an undeclared timer.
#[test]
fn a_service_timer_of_another_key_than_declared_is_an_authoring_bug() {
    let (_, defects) = run_watching_noting(|_| {
        Some(RuleHandleResult::new(vec![RuleAction::cancel(TimerKey::call(TimerType::service(
            MachineId::new(TEST_MACHINE),
            "other",
        )))]))
    });
    assert!(noted(&defects, "emitted an undeclared timer"), "{defects:?}");
}
