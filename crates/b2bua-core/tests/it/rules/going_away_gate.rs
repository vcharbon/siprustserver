// ── The going-away gate (executor) ───────────────────────────────────────────
//
// An asynchronous trigger — a timer fire, a transaction timeout, an
// internal-event fold — on a `Terminating`/`Terminated` call reaches only its
// teardown rules (`RuleDefinition::teardown`); every other candidate is
// absorbed and counted. A peer's message keeps its per-rule filters, and
// `BeginTermination` disarms every service watchdog and deactivates every
// service machine, so the fire is stopped at the source too.
use super::fixtures::*;
use super::*;
use b2bua_core::obligations::ObligationSet;
use call::CdrEventType;

const SVC: MachineId = MachineId::new("svc-gate");
const DEADLINE: TimerType = TimerType::service(SVC, "deadline");
static ARMED: [StateLabel; 1] = [StateLabel::new("Armed")];
static TO_TERMINAL: [(StateLabel, StateLabel); 1] =
    [(StateLabel::new("Armed"), StateLabel::terminal())];
static REJECT_EFFECTS: [Effect; 2] = [
    Effect::Respond { status: 480, label: "answer the caller" },
    Effect::LifecycleCommand { label: "tear the call down" },
];
const SCRUB_REASON: &str = "deadline-scrub";

/// The misbehaving shape: a deadline that answers the caller and tears
/// down, guard-less.
fn reject_on_deadline(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![
        RuleAction::RespondToALeg {
            status: 480,
            reason: "Temporarily Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        },
        RuleAction::BeginTermination {
            reason: Some("deadline".into()),
            cause: TerminationCause::Timeout(call::TimeoutKind::Setup),
            by_leg: None,
        },
        RuleAction::ClearState { machine: SVC },
    ]))
}

/// A teardown rule's shape: the same deadline only books its passing.
fn scrub_on_deadline(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::AddCdrEvent {
        event_type: CdrEventType::Timeout,
        leg_id: "a".into(),
        status_code: None,
        reason: Some(SCRUB_REASON.into()),
    }]))
}

fn watchdog_rule(
    id: &'static str,
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition {
        id,
        layer: SERVICE_LAYER,
        overrides: &[],
        matcher: Match::timer().timer_type(DEADLINE),
        handle,
        machine: Some(SVC),
        active_states: &ARMED,
        transitions: &TO_TERMINAL,
        effects: &REJECT_EFFECTS,
        teardown: false,
    }
}

/// A ringing call with the service seeded `Armed` in `state`.
fn armed_call(state: CallModelState) -> call::Call {
    let mut call = test_call();
    call.state = state;
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    // A ringing callee holds a terminating call until it is settled.
    let mut ringing = b_leg_pending();
    call::testkit::stage_leg(&mut ringing, LegState::Early);
    call = call::helpers::add_b_leg(call, ringing);
    call.sm_cursors.insert(SVC, StateLabel::new("Armed"));
    call
}

fn deadline_fire(call: &call::Call) -> CallEvent {
    CallEvent::Timer {
        key: TimerKey::call(DEADLINE),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    }
}

fn run(rules: &[RuleDefinition], call: &call::Call, event: &CallEvent) -> HandlerResult {
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let ctx = ctx_for(call, event, &config);
    execute_rules(rules, call, &ctx, &mut exec, &ObligationSet::core())
}

fn absorbed_of(result: &HandlerResult) -> Vec<(&'static str, &'static str)> {
    result
        .effects
        .buffered
        .iter()
        .filter_map(|e| match e {
            BufferedObservabilityEffect::GoingAwayAbsorbed { event, rule } => Some((*event, *rule)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_service_watchdog_on_a_going_away_call_is_absorbed_and_counted() {
    for state in [CallModelState::Terminating, CallModelState::Terminated] {
        let call = armed_call(state);
        let rules = vec![watchdog_rule("gate-reject", reject_on_deadline)];
        let result = run(&rules, &call, &deadline_fire(&call));
        assert!(result.effects.outbound.is_empty(), "nothing leaves on {state:?}");
        assert_eq!(result.call.state, state, "the rule never ran: no lifecycle move");
        assert_eq!(
            result.call.sm_cursors.get(&SVC),
            Some(&StateLabel::new("Armed")),
            "the rule never ran: its cursor did not move",
        );
        assert_eq!(
            absorbed_of(&result),
            vec![("timer", "gate-reject")],
            "the absorbed fire is counted once, naming the rule",
        );
    }
}

#[test]
fn a_teardown_rule_still_runs_on_a_going_away_call() {
    let call = armed_call(CallModelState::Terminating);
    let rules = vec![
        watchdog_rule("gate-reject", reject_on_deadline),
        watchdog_rule("gate-scrub", scrub_on_deadline).runs_while_terminating(),
    ];
    let result = run(&rules, &call, &deadline_fire(&call));
    assert!(result.effects.outbound.is_empty(), "the guard-less rule was absorbed");
    assert!(
        result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some(SCRUB_REASON)),
        "the teardown rule ran: {:?}",
        result.call.cdr_events,
    );
    assert_eq!(absorbed_of(&result), vec![("timer", "gate-reject")]);
}

#[test]
fn the_gate_is_inert_on_a_live_call() {
    let call = armed_call(CallModelState::Active);
    let rules = vec![watchdog_rule("gate-reject", reject_on_deadline)];
    let result = run(&rules, &call, &deadline_fire(&call));
    assert!(
        result.effects.outbound.iter().any(|e| e.label.starts_with("480")),
        "on a live call the deadline answers the caller: {:?}",
        result.effects.outbound.iter().map(|e| &e.label).collect::<Vec<_>>(),
    );
    assert!(absorbed_of(&result).is_empty(), "nothing absorbed on a live call");
}

#[test]
fn a_peer_message_is_not_gated() {
    // A machine-bound rule on an in-dialog INFO stays a candidate on a
    // terminating call: the gate covers the call's own clocks, not what a
    // peer says.
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let ctx = ctx_for(&call, &event, &config);
    let rules = vec![sm_rule(handle_to_s1)];
    assert_eq!(pick_ranked(&rules, &call, &ctx).len(), 1, "a peer's INFO reaches the rule");
}

#[test]
fn begin_termination_disarms_service_watchdogs_and_deactivates_machines() {
    // Entering `terminating` retires the call's scope: every service
    // watchdog, every per-leg NoAnswer entry and the call-level cap leave
    // the ledger and the driver, and every service machine's cursor goes;
    // the teardown's own safety net and the lifecycle projection stay.
    let mut call = armed_call(CallModelState::Active);
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
    let deadline = TimerKey::call(DEADLINE);
    let duration = TimerKey::call(TimerType::GlobalDuration);
    for key in [no_answer.clone(), deadline.clone(), duration.clone()] {
        call::helpers::arm_timer(&mut call, key, 5_000);
    }
    call.sm_cursors
        .insert(b2bua_core::rules::invariants::GLOBAL_CALL_MACHINE, StateLabel::new("Active"));
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
        at_ms: 0,
    };
    let config = B2buaConfig::default();
    let ctx = ctx_for(&call, &event, &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let result = exec.execute(
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
        &call,
        &ctx,
    );
    let ledger: Vec<&TimerType> = result.call.timers.iter().map(|t| t.timer_type()).collect();
    assert_eq!(
        ledger,
        vec![&TimerType::TerminatingTimeout],
        "the call's timers left the ledger; the teardown's safety net is armed",
    );
    for key in [&no_answer, &deadline, &duration] {
        assert!(
            result.effects.critical.iter().any(|e| matches!(
                e,
                CriticalStateEffect::CancelTimer { key: cancelled } if cancelled == key
            )),
            "the live fiber {key:?} is cancelled, got {:?}",
            result.effects.critical,
        );
    }
    let machines: Vec<&str> = result.call.sm_cursors.keys().map(|m| m.as_str()).collect();
    assert_eq!(machines, vec!["global-call"], "every service machine is deactivated");
}
