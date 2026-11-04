//! ADR-0016: machine-gated selection, `SetState` and the declared-transition
//! and declared-effect checks over seeded cursors.

use super::fixtures::*;
use super::*;

/// A declared terminal edge: S0 deactivates the machine (ADR-0016 X9).
static SM_TRANSITIONS_TERMINAL: [(StateLabel, StateLabel); 1] =
    [(StateLabel::new("S0"), StateLabel::terminal())];

/// Deactivates the machine (removes the cursor) via `ClearState`.
fn handle_clears_state(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::ClearState {
        machine: MachineId::new(TEST_MACHINE),
    }]))
}

/// Emits an undeclared S0 → S2 move (only S0 → S1 is in `SM_TRANSITIONS`).
fn handle_to_s2_undeclared(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(TEST_MACHINE),
        to: StateLabel::new("S2"),
    }]))
}

/// Emits a tracked `LegMessage` side effect (a final response to a leg).
fn handle_emits_leg_message(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::Respond {
        status: 200,
        reason: "OK".to_string(),
        body: vec![],
        content_type: None,
    }]))
}

/// One declared `LegMessage` effect (ADR-0016 X9 — the rule may respond to a leg).
static SM_EFFECTS_LEG_MESSAGE: [Effect; 1] =
    [Effect::Respond { status: 200, label: "200 → leg" }];

/// Like [`sm_rule`] but with a non-empty declared `effects` list.
fn sm_rule_with_effects(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    effects: &'static [Effect],
) -> RuleDefinition {
    RuleDefinition { effects, ..sm_rule(handle) }
}

/// Like [`sm_rule`] but with a custom declared `transitions` list.
fn sm_rule_with_transitions(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    transitions: &'static [(StateLabel, StateLabel)],
) -> RuleDefinition {
    RuleDefinition { transitions, ..sm_rule(handle) }
}

#[test]
fn machine_rule_is_candidate_only_in_active_state() {
    let mut call = test_call();
    let event = info_event();
    let config = B2buaConfig::default();
    let rules = vec![sm_rule(handle_to_s1)];

    // No cursor seeded → machine dormant → not a candidate.
    {
        let ctx = ctx_for(&call, &event, &config);
        assert!(pick_ranked(&rules, &call, &ctx).is_empty(), "dormant without a cursor");
    }

    // Cursor in `active_states` (S0) → candidate.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert_eq!(
            pick_ranked(&rules, &call, &ctx).first().map(|r| r.id),
            Some("test-sm-rule"),
            "candidate when cursor ∈ active_states"
        );
    }

    // Cursor outside `active_states` (S1) → skipped.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S1"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert!(pick_ranked(&rules, &call, &ctx).is_empty(), "skipped when cursor ∉ active_states");
    }
}

#[test]
fn set_state_moves_cursor_and_gates_the_next_event() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let rules = vec![sm_rule(handle_to_s1)];

    let result = {
        let ctx = ctx_for(&call, &event, &config);
        execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core())
    };
    assert_eq!(
        result.call.sm_cursors.get(&MachineId::new(TEST_MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "SetState moved the cursor to S1"
    );

    // The next event sees the new state: the S0-gated rule no longer fires.
    let next = result.call;
    let ctx2 = ctx_for(&next, &event, &config);
    assert!(
        pick_ranked(&rules, &next, &ctx2).is_empty(),
        "the S0-gated rule is no longer a candidate at S1"
    );
}

/// The turn goes on; the defect is the turn's to report.
#[test]
fn undeclared_transition_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let rules = vec![sm_rule(handle_to_s2_undeclared)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_TRANSITION]);
}

/// The names of the defects `exec`'s turn noted.
fn defect_names(exec: &ActionExecutor) -> Vec<&'static str> {
    exec.turn.notes.defects().iter().map(|d| d.name).collect()
}

/// A tracked side effect the handler emits but the rule did not declare is
/// the drift-check's defect (ADR-0016 X9), the effect analogue of the
/// transition check.
#[test]
fn undeclared_effect_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    // `sm_rule` declares `effects: &[]`, but the handler emits a `LegMessage`.
    let rules = vec![sm_rule(handle_emits_leg_message)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_EFFECT]);
}

/// Declaring the matching `LegMessage` effect satisfies the drift-check — the
/// same emit no longer trips it (the by-category `emitted ⊆ declared` contract).
#[test]
fn declared_effect_passes_the_drift_check() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let rules = vec![sm_rule_with_effects(handle_emits_leg_message, &SM_EFFECTS_LEG_MESSAGE)];

    let ctx = ctx_for(&call, &event, &config);
    // No defect: the emitted Respond is a declared LegMessage.
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert!(defect_names(&exec).is_empty());
}

/// `ClearState` (machine deactivation) removes the cursor, and with a declared
/// `S0 => terminal` edge the transition drift-check accepts it (ADR-0016 X9).
#[test]
fn declared_terminal_clear_state_deactivates_machine() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let rules = vec![sm_rule_with_transitions(handle_clears_state, &SM_TRANSITIONS_TERMINAL)];

    let ctx = ctx_for(&call, &event, &config);
    let r = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    // The machine is deactivated: its cursor is gone.
    assert!(!r.call.sm_cursors.contains_key(&MachineId::new(TEST_MACHINE)));
}

/// A `ClearState` whose `S0 => terminal` edge is **not** declared is the
/// transition drift-check's defect (only `S0 => S1` is in `SM_TRANSITIONS`).
#[test]
fn undeclared_terminal_clear_state_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let rules = vec![sm_rule(handle_clears_state)]; // transitions: S0 => S1 only

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_TRANSITION]);
}
