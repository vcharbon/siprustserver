//! ADR-0016 — the public Rule SDK is a self-contained authoring surface.
//!
//! This test crate depends on **`b2bua-sdk` only** (no `b2bua`, not even `call`
//! directly — the framework types are reached through `b2bua_sdk::rules`). A
//! service authored entirely against that surface must compile and produce
//! correct rule metadata; the `watchdog` service, with a timer, a direction and
//! a leg state, is checked by compiling alone. It is the in-crate counterpart of the out-of-tree
//! boundary check the `announcement` crate makes.

use b2bua_sdk::rules::{
    Effect, EffectKind, MachineId, Match, Method, RuleContext, RuleDefinition, RuleHandleResult,
    ServiceDef, StateLabel, SERVICE_LAYER,
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

/// A service that guards its media leg with a timer of its own: armed on the
/// callee's early answer, matched on its fire. It is a compile-only check: every
/// name it uses comes from `b2bua_sdk::rules`, and the build is its test.
mod watchdog {
    use b2bua_sdk::rules::{
        leg_kind, Direction, Effect, Leg, LegKind, LegState, Match, RuleAction, RuleCall,
        RuleContext, RuleHandleResult, TimerDelay, TimerKey, TimerScopeKind, TimerType,
    };
    use b2bua_sdk::{define_service, sm_rule};

    const WATCHDOG: TimerType = TimerType::service(WATCHDOG_MACHINE, "clip-watchdog");

    /// The live media leg of the call.
    fn media_leg<'a>(call: &RuleCall<'a>) -> Option<&'a Leg> {
        call.b_legs()
            .iter()
            .find(|l| leg_kind(l) == LegKind::Media && l.state() != LegState::Terminated)
    }

    fn on_early(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let leg = media_leg(&ctx.call)?;
        let key = TimerKey::leg(WATCHDOG, leg.leg_id.clone());
        Some(RuleHandleResult::new(vec![RuleAction::arm(key, TimerDelay::secs(10))]))
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

const _: fn() -> ServiceDef = watchdog::service_def;

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
