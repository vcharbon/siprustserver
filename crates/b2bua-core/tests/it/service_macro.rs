//! ADR-0016 — `define_service!` / `sm_rule!` macro + `init` hook.
//!
//! A test-only **stub service** (`S0 → S1`, one `init`, one state-gated rule)
//! exercised end-to-end with no real service: the macro generates the state
//! enum, the machine, the rules and the init; the call's setup seeds the cursor
//! (S0) and the data backing; the rule fires in S0, `SetState`s to S1, and
//! declines in S1 — proving macro, init and selection compose.

use b2bua_core::rules::{
    compose_rules, default_rules, Match, RuleAction, RuleCall, RuleContext, RuleDefinition,
    RuleHandleResult, ServiceDef, ServiceSeed,
};
use b2bua_core::turn_driver::{Driver, Step};
use b2bua_sdk::{define_service, sm_rule};
use call::{Call, MachineId, StateLabel};
use sip_message::Method;

use crate::calls::bridged;

/// The test-only stub service.
mod stub {
    use super::*;

    define_service! {
        id: "stub",
        machine: STUB_MACHINE,
        states: StubState { S0, S1 },
        // Always applicable in the test; a real service decides applicability
        // here (returning `None` to stay dormant). Seeds cursor=S0 + a data
        // marker (using `callback_context` as the stub's "data backing").
        init: |_call: &RuleCall| {
            Some(
                ServiceSeed::new(StubState::S0.label())
                    .with_data(|c| c.callback_context = Some("stub-seeded".into())),
            )
        },
        rules: [ advance() ],
    }

    /// In S0, on an INFO, advance to S1.
    fn advance() -> RuleDefinition {
        sm_rule! {
            id: "stub-advance",
            machine: STUB_MACHINE,
            active: [ StubState::S0 ],
            transitions: [ StubState::S0 => StubState::S1 ],
            // Pure transition: SetState is a cursor move, not a tracked effect.
            effects: [],
            matcher: Match::request().method("INFO"),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![RuleAction::SetState {
                    machine: STUB_MACHINE,
                    to: StubState::S1.label(),
                }]))
            },
        }
    }
}

#[test]
fn stub_service_init_seeds_then_rule_advances_then_declines() {
    let machine = MachineId::new("stub");
    let cursor = |c: &Call| c.sm_cursors.get(&machine).map(StateLabel::as_str).map(str::to_string);
    let driver = Driver::new().with_service(stub::service_def());
    let mut call = bridged(&driver);
    assert_eq!(cursor(call.call()).as_deref(), Some("S0"), "init seeded cursor=S0");
    assert_eq!(
        call.call().callback_context.as_deref(),
        Some("stub-seeded"),
        "init installed the data backing"
    );

    let advanced = call.turned(Step::Sends("a", Method::Info));
    assert_eq!(advanced.rule, Some("stub-advance"), "the rule claims the INFO in S0");
    assert_eq!(cursor(call.call()).as_deref(), Some("S1"), "SetState moved cursor to S1");

    let declined = call.turned(Step::Sends("a", Method::Info));
    assert_ne!(declined.rule, Some("stub-advance"), "the S0-gated rule declines in S1");
}

/// A dormant service (`init` → `None`) costs a vanilla call nothing: no cursor,
/// no data, and `compose_rules` with no services equals the bare core list.
#[test]
fn dormant_service_leaves_call_untouched() {
    fn dormant_init(_: &RuleCall) -> Option<ServiceSeed> {
        None
    }
    let dormant = ServiceDef { id: "dormant", init: dormant_init, rules: Vec::new };
    let with = Driver::new().with_service(dormant);
    let without = Driver::new();
    assert_eq!(with.dial().call(), without.dial().call(), "dormant service touches nothing");

    // Empty composition is exactly the core list.
    let composed = compose_rules(&[], default_rules());
    assert_eq!(composed.len(), default_rules().len());
}

/// A seed's configured part reads the worker's service parameters: its data
/// write and its actions follow what the composition inserted.
#[test]
fn a_configured_seed_reads_the_service_parameters() {
    struct Marker(&'static str);

    fn configured_init(_: &RuleCall) -> Option<ServiceSeed> {
        Some(ServiceSeed::new(StateLabel::new("S0")).with_configured(|config, c| {
            let Some(Marker(text)) = config.service_params.get::<Marker>() else {
                return Vec::new();
            };
            c.callback_context = Some((*text).to_string());
            vec![RuleAction::SetState {
                machine: MachineId::new("configured"),
                to: StateLabel::new("S1"),
            }]
        }))
    }
    let configured = ServiceDef { id: "configured", init: configured_init, rules: Vec::new };
    let driver = Driver::new()
        .with_config(|c| c.service_params.insert(Marker("from-config")))
        .with_service(configured);
    let call = driver.dial();
    let seeded = call.call();
    assert_eq!(seeded.callback_context.as_deref(), Some("from-config"), "the data write");
    assert_eq!(
        seeded.sm_cursors.get(&MachineId::new("configured")).map(StateLabel::as_str),
        Some("S1"),
        "the configured action was folded"
    );
}
