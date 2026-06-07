//! Rule selection + execution — port of `Matcher.ts` (`pickRanked`) +
//! `RuleExecutor.ts`. First handler returning `Some` wins; its actions run
//! through the [`ActionExecutor`], then termination is finalized + invariants
//! enforced. No candidate → the default handler.

use std::collections::HashSet;

use call::Call;

use crate::effects::HandlerResult;

use super::actions::ActionExecutor;
use super::invariants;
use super::model::{RuleContext, RuleDefinition};

/// A machine-bound rule (ADR-0016 X1) is a candidate only when its owner
/// machine's cursor is one of its `active_states`. A machine-less core rule is
/// always a candidate (an unseeded machine keeps its rules dormant — selection
/// costs a vanilla call nothing).
fn machine_active(r: &RuleDefinition, call: &Call) -> bool {
    match &r.machine {
        None => true,
        Some(m) => call
            .sm_cursors
            .get(m)
            .is_some_and(|cursor| r.active_states.contains(cursor)),
    }
}

/// Filter rules by columns + filter predicate, drop overridden rules, and sort
/// by layer (desc) then registration order (asc, stable).
pub fn pick_ranked<'a>(rules: &'a [RuleDefinition], ctx: &RuleContext) -> Vec<&'a RuleDefinition> {
    let mut candidates: Vec<(usize, &RuleDefinition)> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.matcher.accepts_columns(ctx)
                && machine_active(r, ctx.call)
                && r.matcher.filter.is_none_or(|f| f(ctx))
        })
        .collect();

    let overridden: HashSet<&str> = candidates
        .iter()
        .flat_map(|(_, r)| r.overrides.iter().copied())
        .collect();
    candidates.retain(|(_, r)| !overridden.contains(r.id));

    candidates.sort_by(|a, b| b.1.layer.cmp(&a.1.layer).then(a.0.cmp(&b.0)));
    candidates.into_iter().map(|(_, r)| r).collect()
}

/// Run the rule chain for `ctx`. The first matching rule that returns `Some`
/// handles the event; otherwise `default` is invoked.
pub fn execute_rules(
    rules: &[RuleDefinition],
    ctx: &RuleContext,
    exec: &ActionExecutor,
    default: fn(&RuleContext) -> HandlerResult,
) -> HandlerResult {
    for rule in pick_ranked(rules, ctx) {
        if let Some(outcome) = (rule.handle)(ctx) {
            let before = ctx.call.clone();
            let result = exec.execute(&outcome.actions, ctx);
            check_declared_transition(rule, &before.sm_cursors, &result.call.sm_cursors);
            let result = invariants::finalize(result);
            return invariants::enforce(&before, result);
        }
    }
    default(ctx)
}

/// Assert any cursor move the winning rule caused on its **own** machine is a
/// declared `(from, to)` edge (ADR-0016 X1). Keeps the generated diagram
/// exhaustive and catches authoring bugs. Debug builds panic; release builds log
/// and proceed — an undeclared transition must never panic a live worker.
fn check_declared_transition(
    rule: &RuleDefinition,
    before: &std::collections::BTreeMap<call::MachineId, call::StateLabel>,
    after: &std::collections::BTreeMap<call::MachineId, call::StateLabel>,
) {
    let Some(machine) = rule.machine.as_ref() else {
        return;
    };
    let from = before.get(machine);
    let to = after.get(machine);
    if from == to {
        return; // no move (or the rule SetState'd to the same label).
    }
    // A machine-bound rule only fires from a seeded cursor (the `machine_active`
    // gate), so `from` is always present here.
    let declared = match (from, to) {
        (Some(f), Some(t)) => rule.transitions.iter().any(|(df, dt)| df == f && dt == t),
        _ => false,
    };
    if !declared {
        if cfg!(debug_assertions) {
            panic!(
                "rule '{}' caused an undeclared transition on machine '{}': {:?} -> {:?} \
                 (declare it in the rule's `transitions`)",
                rule.id,
                machine.as_str(),
                from.map(call::StateLabel::as_str),
                to.map(call::StateLabel::as_str),
            );
        } else {
            eprintln!(
                "WARN: rule '{}' caused an undeclared transition on machine '{}': {:?} -> {:?}",
                rule.id,
                machine.as_str(),
                from.map(call::StateLabel::as_str),
                to.map(call::StateLabel::as_str),
            );
        }
    }
}
