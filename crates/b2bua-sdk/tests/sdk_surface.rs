//! ADR-0016 slice 6 — the public Rule SDK is a self-contained authoring surface.
//!
//! This test crate depends on **`b2bua-sdk` only** (no `b2bua`, not even `call`
//! directly — the framework types are reached through `b2bua_sdk::rules`). A
//! service authored entirely against that surface must compile and produce
//! correct rule metadata. It is the in-crate rehearsal of slice 8's real
//! out-of-tree boundary check (the `announcement` crate).

use b2bua_sdk::rules::{
    Match, MachineId, RuleContext, RuleDefinition, RuleHandleResult, StateLabel, SERVICE_LAYER,
};
use b2bua_sdk::{define_service, sm_rule};

/// A handler reachable through the SDK surface alone (no `b2bua` types).
fn advance(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    None
}

mod stub {
    use super::*;
    use b2bua_sdk::rules::Call;

    define_service! {
        id: "stub",
        machine: STUB_MACHINE,
        states: StubState { S0, S1 },
        init: |_call: &Call| None,
        rules: [
            sm_rule! {
                id: "stub-advance",
                machine: STUB_MACHINE,
                active: [ StubState::S0 ],
                transitions: [ StubState::S0 => StubState::S1 ],
                matcher: Match::request().method("INFO"),
                handle: advance,
            },
        ],
    }
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
}

#[test]
fn service_def_carries_the_id_and_rule_factory() {
    // The `init` field's type (`fn(&Call) -> Option<ServiceSeed>`) is proven by
    // compilation; here we pin the id and the rule factory the registry composes.
    let def = stub::service_def();
    assert_eq!(def.id, "stub");
    assert_eq!((def.rules)().len(), 1);
}
