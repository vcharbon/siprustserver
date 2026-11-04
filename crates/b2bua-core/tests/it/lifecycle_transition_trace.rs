//! A cursor move a lifecycle handler makes reaches the trace (ADR-0026).
//!
//! A service hearing a lifecycle event the engine delivers after a turn's
//! claim may move its own machine's cursor. That move is a state-machine
//! transition like a claim's, and a traced call's turn notes it as one
//! (ADR-0052 §4): a `rule.transition` naming the handler, its machine and the
//! edge, beside the `rule.lifecycle` delivery itself.

use b2bua_core::rules::{
    Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef, ServiceSeed,
    SERVICE_LAYER,
};
use b2bua_core::turn_driver::{Driver, Step};
use b2bua_sdk::model::RuleCall;
use call::{MachineId, StateLabel};

const MACHINE: &str = "listening-machine";

static ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];
static S0_TO_S1: [(StateLabel, StateLabel); 1] = [(StateLabel::new("S0"), StateLabel::new("S1"))];

/// Hears a leg end and moves its own cursor S0 → S1.
fn on_leg_ended(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(MACHINE),
        to: StateLabel::new("S1"),
    }]))
}

/// A machine-bound leg-end listener, active in S0, declaring S0 → S1.
fn listener() -> RuleDefinition {
    RuleDefinition {
        id: "test-leg-end-listener",
        layer: SERVICE_LAYER,
        overrides: &[],
        matcher: Match::leg_ended(),
        handle: on_leg_ended,
        machine: Some(MachineId::new(MACHINE)),
        active_states: &ACTIVE_S0,
        transitions: &S0_TO_S1,
        effects: &[],
        teardown: false,
    }
}

/// Claims the internal event and refuses the caller, which ends her leg.
fn ending_claimer() -> RuleDefinition {
    RuleDefinition::core(
        "test-ending-claimer",
        SERVICE_LAYER,
        &[],
        Match::internal_event().topic("end-a"),
        |_| {
            Some(RuleHandleResult::new(vec![RuleAction::RespondToALeg {
                status: 486,
                reason: "Busy Here".into(),
                header_updates: vec![],
                contacts: vec![],
            }]))
        },
    )
}

/// The listening service: its machine seeded at S0 on every call.
fn listening() -> ServiceDef {
    fn init(_: &RuleCall) -> Option<ServiceSeed> {
        Some(ServiceSeed::new(StateLabel::new("S0")))
    }
    ServiceDef { id: MACHINE, init, rules: || vec![listener(), ending_claimer()] }
}

#[test]
fn a_lifecycle_handlers_cursor_move_is_on_the_trace() {
    let driver = Driver::new().sampled().with_service(listening());
    let mut call = driver.dial();
    let at = |c: &call::Call| c.sm_cursors.get(&MachineId::new(MACHINE)).cloned();
    assert_eq!(at(call.call()), Some(StateLabel::new("S0")), "the service seeded its machine");
    let end_a =
        Step::Internal { leg: "a", topic: "end-a", outcome: "ok", payload: serde_json::json!({}) };
    let turn = call.turned(end_a);
    assert_eq!(turn.rule, Some("test-ending-claimer"));
    let facts = turn.notes.trace_facts().to_vec();
    assert_eq!(
        call.call().sm_cursors.get(&MachineId::new(MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "the listener moved its cursor",
    );
    let of = |kind: &str| facts.iter().filter(|f| f.kind == kind).collect::<Vec<_>>();
    assert!(
        of("rule.lifecycle").iter().any(|f| f.detail == "test-leg-end-listener leg-ended"),
        "the call is traced: the delivery itself is on the trace: {facts:?}",
    );
    assert!(
        of("rule.transition")
            .iter()
            .any(|f| f.detail == "test-leg-end-listener listening-machine: S0 -> S1"),
        "the listener's move is a traced transition: {facts:?}",
    );
}
