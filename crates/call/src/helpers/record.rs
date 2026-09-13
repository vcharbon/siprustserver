//! Call-level helpers that touch no specific leg or dialog: CDR append, rule
//! deactivation, and the SM-cursor debug rendering.

use crate::model::{Call, CdrEvent};

/// Append a CDR event, stamped with the count of decisions applied so far
/// (`Call::decision_ordinal`).
pub fn add_cdr_event(mut call: Call, mut event: CdrEvent) -> Call {
    event.decision_ordinal = call.decision_ordinal;
    call.cdr_events.push(event);
    call
}

/// Deactivate a rule (set `active = false`); preserves the entry for tracing.
pub fn deactivate_rule(mut call: Call, rule_id: &str) -> Call {
    let mut rules = call.active_rules.take().unwrap_or_default();
    for r in &mut rules {
        if r.id == rule_id {
            r.active = false;
        }
    }
    call.active_rules = Some(rules);
    call
}

/// Render a call's live state-machine cursors as one compact, deterministic
/// line — e.g. `global-call=Active transfer=CRinging`. Machines are emitted in
/// [`MachineId`](crate::model::MachineId) order (the `BTreeMap` is already
/// sorted), so the output is stable for logs/snapshots, and an empty map (no
/// active machine) renders as `-`. Read-only: the `SetState` action and the
/// finalize projections remain the only writers of `sm_cursors`. The live
/// distribution is the `b2bua_sm_cursors` gauge.
pub fn dump_cursors(call: &Call) -> String {
    if call.sm_cursors.is_empty() {
        return "-".to_string();
    }
    call.sm_cursors
        .iter()
        .map(|(machine, state)| format!("{}={}", machine.as_str(), state.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}
