//! ADR-0016 — the public Rule SDK is a self-contained authoring surface.
//!
//! This test crate depends on **`b2bua-sdk` only** (no `b2bua`, not even `call`
//! directly — the framework types are reached through `b2bua_sdk::rules`). A
//! service authored entirely against that surface must compile and produce
//! correct rule metadata. It is the in-crate counterpart of the out-of-tree
//! boundary check the `announcement` crate makes.

use b2bua_sdk::rules::{
    Direction, Effect, EffectKind, LegState, MachineId, Match, Method, RuleAction, RuleContext,
    RuleDefinition, RuleHandleResult, StateLabel, TimerKey, TimerScopeKind, TimerType,
    SERVICE_LAYER,
};
use b2bua_sdk::{define_service, sm_rule};

/// A handler reachable through the SDK surface alone (no `b2bua` types).
fn advance(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    None
}

mod stub {
    use super::*;
    use b2bua_sdk::rules::RuleCall;

    define_service! {
        id: "stub",
        machine: STUB_MACHINE,
        states: StubState { S0, S1 },
        init: |_call: &RuleCall| None,
        rules: [
            sm_rule! {
                id: "stub-advance",
                machine: STUB_MACHINE,
                active: [ StubState::S0 ],
                transitions: [ StubState::S0 => StubState::S1 ],
                effects: [ Effect::Originate { method: Method::Info, label: "INFO → leg" } ],
                matcher: Match::request().method("INFO"),
                handle: advance,
            },
        ],
    }
}

/// A service that guards a leg with a timer of its own: armed on the callee's
/// early answer, matched on its fire. Every name it uses comes from
/// `b2bua_sdk::rules`.
mod watchdog {
    use b2bua_sdk::rules::{
        Direction, Effect, LegState, Match, RuleAction, RuleCall, RuleContext, RuleHandleResult,
        TimerDelay, TimerKey, TimerScopeKind, TimerType,
    };
    use b2bua_sdk::{define_service, sm_rule};

    pub const WATCHDOG: TimerType = TimerType::service(WATCHDOG_MACHINE, "clip-watchdog");

    /// The action that guards `leg_id` for 10 s.
    pub fn arm(leg_id: &str) -> RuleAction {
        RuleAction::arm(TimerKey::leg(WATCHDOG, leg_id), TimerDelay::secs(10))
    }

    fn on_early(ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![arm(ctx.source_leg_id)]))
    }

    fn on_fire(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        None
    }

    define_service! {
        id: "watchdog",
        machine: WATCHDOG_MACHINE,
        states: WatchdogState { Guarding },
        init: |_call: &RuleCall| None,
        rules: [
            sm_rule! {
                id: "watchdog-arm",
                machine: WATCHDOG_MACHINE,
                active: [ WatchdogState::Guarding ],
                transitions: [],
                effects: [
                    Effect::GuardTimer { timer: WATCHDOG, scope: TimerScopeKind::Leg, label: "clip watchdog" },
                ],
                matcher: Match::response()
                    .method("INVITE")
                    .status_class(1)
                    .direction(Direction::FromB)
                    .leg_states(&[LegState::Early]),
                handle: on_early,
            },
            sm_rule! {
                id: "watchdog-fire",
                machine: WATCHDOG_MACHINE,
                active: [ WatchdogState::Guarding ],
                transitions: [],
                effects: [],
                matcher: Match::timer().service_timers(WATCHDOG_MACHINE),
                handle: on_fire,
            },
        ],
    }
}

#[test]
fn a_service_arms_a_leg_timer_of_its_own() {
    let RuleAction::ScheduleTimer { key, delay } = watchdog::arm("b1") else {
        panic!("RuleAction::arm builds a ScheduleTimer");
    };
    assert_eq!(
        key,
        TimerKey::leg(TimerType::service(MachineId::new("watchdog"), "clip-watchdog"), "b1")
    );
    assert_eq!(key.leg_id(), Some("b1"));
    assert_eq!(delay.as_millis(), 10_000);
}

#[test]
fn a_timer_rule_states_its_columns_and_declares_its_timer() {
    let rules = watchdog::rules();
    let arm = rules.iter().find(|r| r.id == "watchdog-arm").expect("the arm rule");
    assert_eq!(arm.matcher.direction, Some(Direction::FromB));
    assert_eq!(arm.matcher.leg_state, Some(vec![LegState::Early]));
    assert!(matches!(
        arm.effects,
        [Effect::GuardTimer { timer, scope: TimerScopeKind::Leg, .. }] if *timer == watchdog::WATCHDOG
    ));
    let fire = rules.iter().find(|r| r.id == "watchdog-fire").expect("the fire rule");
    assert_eq!(fire.matcher.timer_service, Some(MachineId::new("watchdog")));
}

#[test]
fn macro_yields_machine_id_and_state_labels() {
    assert_eq!(stub::STUB_MACHINE, MachineId::new("stub"));
    assert_eq!(stub::StubState::S0.label(), StateLabel::new("S0"));
    assert_eq!(stub::StubState::S1.label(), StateLabel::new("S1"));
}

#[test]
fn sm_rule_populates_the_machine_columns() {
    let rules: Vec<RuleDefinition> = stub::rules();
    assert_eq!(rules.len(), 1);
    let r = &rules[0];
    assert_eq!(r.id, "stub-advance");
    assert_eq!(r.layer, SERVICE_LAYER);
    assert_eq!(r.machine, Some(MachineId::new("stub")));
    assert_eq!(r.active_states, &[StateLabel::new("S0")]);
    assert_eq!(r.transitions, &[(StateLabel::new("S0"), StateLabel::new("S1"))]);
    // The effects surface is authored + introspected through the SDK alone.
    assert_eq!(r.effects.len(), 1);
    assert_eq!(r.effects[0].kind(), EffectKind::LegMessage);
}

#[test]
fn service_def_carries_the_id_and_rule_factory() {
    // The `init` field's type (`fn(&RuleCall) -> Option<ServiceSeed>`, the
    // narrow read view — ADR-0020 X8) is proven by compilation; here we pin the
    // id and the rule factory the registry composes.
    let def = stub::service_def();
    assert_eq!(def.id, "stub");
    assert_eq!((def.rules)().len(), 1);
}
