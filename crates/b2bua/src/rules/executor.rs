//! Rule selection + execution — port of `Matcher.ts` (`pickRanked`) +
//! `RuleExecutor.ts`. First handler returning `Some` wins; its actions run
//! through the [`ActionExecutor`], then termination is finalized + invariants
//! enforced. A handler that only observes writes its call-ext slices and its
//! own machine's bookkeeping, and the chain goes on
//! ([`RuleHandleResult::observe`]). No candidate → the default handler.
//! Selection gates on the call's lifecycle too: a call already going away
//! makes no forward progress on its own clock, so an asynchronous trigger
//! reaches only its teardown rules there (`RuleDefinition::teardown`) and
//! every other candidate is absorbed and counted.

use std::collections::HashSet;

use call::{Call, CallModelState, TimerType};

use crate::effects::{BufferedObservabilityEffect, HandlerEffects, HandlerResult};
use crate::obligations::ObligationSet;

use super::actions::ActionExecutor;
use super::invariants;
use b2bua_sdk::model::{
    EffectKind, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
};

/// A machine-bound rule (ADR-0016 X1) is a candidate only when its owner
/// machine's cursor is one of its `active_states`. A machine-less core rule is
/// always a candidate (an unseeded machine keeps its rules dormant — selection
/// costs a vanilla call nothing).
fn machine_active(r: &RuleDefinition, call: &Call) -> bool {
    match &r.machine {
        None => true,
        Some(m) => call.sm_cursors.get(m).is_some_and(|cursor| r.active_states.contains(cursor)),
    }
}

/// The going-away gate: an asynchronous trigger on a `Terminating` /
/// `Terminated` call reaches only teardown rules. Every other rule that would
/// have matched is absorbed — the call makes no forward progress on its own
/// clock (no failover consult, no fresh leg, no final on a transaction that
/// already carries one) — and named in `absorbed` so the fire is counted.
fn going_away_gate(r: &RuleDefinition, call: &Call, ctx: &RuleContext) -> bool {
    r.teardown
        || !ctx.event.is_asynchronous_trigger()
        || !matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
}

/// What selection found for one event: the ranked candidates, and the rules
/// the going-away gate absorbed (ids, in registration order).
struct Selection<'a> {
    ranked: Vec<&'a RuleDefinition>,
    absorbed: Vec<&'static str>,
}

/// Filter rules by columns + filter predicate, apply the going-away gate,
/// drop overridden rules, and sort by layer (desc) then registration order
/// (asc, stable). The gate runs before overrides, so an absorbed rule
/// displaces nothing: the teardown rule it would have overridden still runs.
/// `call` is the authoritative full struct (machine gating reads
/// `sm_cursors`); rules themselves see only the narrow `ctx.call` view
/// (ADR-0020 X8).
fn select<'a>(rules: &'a [RuleDefinition], call: &Call, ctx: &RuleContext) -> Selection<'a> {
    let mut absorbed = Vec::new();
    let mut candidates: Vec<(usize, &RuleDefinition)> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.matcher.accepts_columns(ctx)
                && machine_active(r, call)
                && r.matcher.filter.is_none_or(|f| f(ctx))
        })
        .filter(|(_, r)| {
            let admitted = going_away_gate(r, call, ctx);
            if !admitted {
                absorbed.push(r.id);
            }
            admitted
        })
        .collect();

    let overridden: HashSet<&str> =
        candidates.iter().flat_map(|(_, r)| r.overrides.iter().copied()).collect();
    candidates.retain(|(_, r)| !overridden.contains(r.id));

    candidates.sort_by(|a, b| b.1.layer.cmp(&a.1.layer).then(a.0.cmp(&b.0)));
    Selection { ranked: candidates.into_iter().map(|(_, r)| r).collect(), absorbed }
}

/// The ranked candidates for `ctx` on `call` (see `select`).
pub fn pick_ranked<'a>(
    rules: &'a [RuleDefinition],
    call: &Call,
    ctx: &RuleContext,
) -> Vec<&'a RuleDefinition> {
    select(rules, call, ctx).ranked
}

/// Run the rule chain for `ctx` over the authoritative `call`. The first
/// matching rule that returns `Some` handles the event; a result that only
/// observes ([`RuleHandleResult::observe`]) writes its call-ext slices and its
/// own machine's bookkeeping, and the chain goes on over the call with those
/// writes. The observers' effects precede the claiming rule's, so a teardown
/// the claim begins supersedes a timer an observer armed. No candidate → the
/// call as the observers left it, with their effects only. A fire the
/// going-away gate absorbed is counted either way.
pub fn execute_rules(
    rules: &[RuleDefinition],
    call: &Call,
    ctx: &RuleContext,
    exec: &ActionExecutor,
    obligations: &ObligationSet,
) -> HandlerResult {
    let selection = select(rules, call, ctx);
    // The call as the observers so far left it; `None` until one wrote.
    let mut observed: Option<Call> = None;
    let mut observed_fx = HandlerEffects::new();
    for rule in selection.ranked {
        let call = observed.as_ref().unwrap_or(call);
        let rule_ctx = RuleContext { call: RuleCall::new(call), ..*ctx };
        let ctx = &rule_ctx;
        if let Some(outcome) = (rule.handle)(ctx) {
            if outcome.observes {
                report_diagnostics(rule, call, &outcome);
                let next = apply_observation(rule, call, ctx, exec, &outcome.actions);
                crate::trace::emit::rule_observed(&next.call, exec.now_ms, rule.id);
                record_cursor_moves(rule, call, &next.call, exec.now_ms);
                observed_fx.extend(next.effects);
                observed = Some(next.call);
                continue;
            }
            let before = call.clone();
            report_diagnostics(rule, call, &outcome);
            check_declared_effects(rule, &outcome.actions);
            let result = exec.execute(&outcome.actions, call, ctx);
            // The rule's OWN cursor move is checked against what the rule
            // produced — the projections, the terminal fold below and the
            // machine deactivation termination performs belong to the engine,
            // not to the rule that triggered them.
            let torn_down = before.state != result.call.state
                && matches!(
                    result.call.state,
                    CallModelState::Terminating | CallModelState::Terminated
                );
            check_declared_transition(rule, &before.sm_cursors, &result.call.sm_cursors, torn_down);
            let result = invariants::finalize(result);
            let mut enforced = invariants::enforce(
                obligations,
                &before,
                result,
                exec.now_ms,
                invariants::UnansweredCaller::Answer(&exec.config.minted_final_advertisement),
            );
            // Recorded from the FINAL call, so the trace carries what finalize
            // and enforce synthesized too — the ADR-0022 unanswered-a-leg 503
            // otherwise shows on a traced call as a `sip.out` with no matching
            // `call.transition`.
            record_transitions(rule, &before, &enforced.call, exec.now_ms);
            note_absorbed(&mut enforced.effects, call, ctx, &selection.absorbed);
            observed_fx.extend(enforced.effects);
            enforced.effects = observed_fx;
            return enforced;
        }
    }
    let call = observed.as_ref().unwrap_or(call);
    let mut result = HandlerResult { call: call.clone(), effects: observed_fx };
    note_absorbed(&mut result.effects, call, ctx, &selection.absorbed);
    result
}

/// Apply an observation to `call`: its call-ext writes, then its own
/// machine's bookkeeping ([`is_own_bookkeeping`]) through the executor, held
/// to the rule's declared effects and edges as a claim's actions are. Any
/// other action is not an observation's to take: an authoring bug, which
/// panics under `debug_assertions` (as an undeclared effect does) and is
/// dropped and logged in release, so the claiming rule's handling stays the
/// turn's only other effect.
fn apply_observation(
    rule: &RuleDefinition,
    call: &Call,
    ctx: &RuleContext,
    exec: &ActionExecutor,
    actions: &[RuleAction],
) -> HandlerResult {
    let mut call = call.clone();
    let mut bookkeeping = Vec::new();
    for action in actions {
        match action {
            RuleAction::MergeCallExt { ext } => {
                for (key, value) in ext {
                    let value = (!value.is_null()).then(|| value.clone());
                    call = call::helpers::set_call_ext(call, key, value);
                }
            }
            own if is_own_bookkeeping(rule, own) => bookkeeping.push(own.clone()),
            other => {
                if cfg!(debug_assertions) {
                    panic!(
                        "rule '{}' observed with a {:?} action (an observation writes call ext \
                         and its own machine's bookkeeping only)",
                        rule.id,
                        other.effect_kind(),
                    );
                }
                tracing::error!(
                    call_ref = %call.call_ref,
                    rule = %rule.id,
                    action = ?other,
                    "an observing rule may only write call ext and its own machine's bookkeeping; \
                     action dropped"
                );
            }
        }
    }
    if bookkeeping.is_empty() {
        return HandlerResult::new(call);
    }
    check_declared_effects(rule, &bookkeeping);
    let ctx = RuleContext { call: RuleCall::new(&call), ..*ctx };
    let result = exec.execute(&bookkeeping, &call, &ctx);
    check_declared_transition(rule, &call.sm_cursors, &result.call.sm_cursors, false);
    result
}

/// The observing rule's own bookkeeping: a move of its machine's cursor, or
/// the arming or cancelling of a service timer its machine owns
/// ([`TimerType::Service`] keyed by that machine). A machine-less rule owns none.
fn is_own_bookkeeping(rule: &RuleDefinition, action: &RuleAction) -> bool {
    let Some(machine) = rule.machine.as_ref() else {
        return false;
    };
    match action {
        RuleAction::SetState { machine: m, .. } | RuleAction::ClearState { machine: m } => {
            m == machine
        }
        RuleAction::ScheduleTimer { timer_type: TimerType::Service { service_id, .. }, .. } => {
            service_id == machine
        }
        RuleAction::CancelTimer { id } => {
            // Every id of the machine's service timers starts with this recipe's.
            id.starts_with(&TimerType::service_owned(machine.clone(), String::new()).timer_id(None))
        }
        _ => false,
    }
}

/// Count a fire the going-away gate absorbed: one effect per turn, naming
/// the event and the highest-registered rule it kept from running (the
/// router counts it as `going_away_absorbed`). Nothing when the gate absorbed
/// nothing.
fn note_absorbed(
    fx: &mut HandlerEffects,
    call: &Call,
    ctx: &RuleContext,
    absorbed: &[&'static str],
) {
    let Some(rule) = absorbed.first() else {
        return;
    };
    tracing::debug!(
        call_ref = %call.call_ref,
        event = ctx.event.kind(),
        rules = ?absorbed,
        "asynchronous trigger absorbed on a going-away call"
    );
    fx.buffered
        .push(BufferedObservabilityEffect::GoingAwayAbsorbed { event: ctx.event.kind(), rule });
}

/// Report the inputs the winning rule refused to read. This is the engine end
/// of the rule SDK's diagnostic seam: a rule that cannot read
/// an input says so here and emits its own refusal, instead of choosing between
/// silence and acting as if the input were fine. The diagnostic itself produces
/// no wire traffic — the rule's actions do.
fn report_diagnostics(rule: &RuleDefinition, call: &Call, outcome: &RuleHandleResult) {
    for d in &outcome.diagnostics {
        tracing::warn!(call_ref = %call.call_ref, rule = %rule.id, detail = %d, "rule refused an input");
    }
}

/// Record what the winning rule's turn did on a traced call (ADR-0026): which
/// rule handled the event, every state-machine cursor the turn moved, and the
/// call's own lifecycle transition. `after` is the call as the turn LEAVES it —
/// invariant finalization and enforcement included — so a transition those
/// layers synthesize reaches the trace attributed to the turn that caused it.
/// Guarded — an unsampled call reads one `Option<bool>` and returns, evaluating
/// no format argument.
fn record_transitions(rule: &RuleDefinition, before: &Call, after: &Call, now_ms: i64) {
    if !crate::trace::sampled(after) {
        return;
    }
    crate::trace::emit::rule_fired(after, now_ms, rule.id);
    record_cursor_moves(rule, before, after, now_ms);
    if before.state != after.state {
        crate::trace::emit::context_transition(after, now_ms, before.state, after.state);
    }
}

/// Record on a traced call every state-machine cursor `rule`'s turn moved
/// between `before` and `after`, a claim's or an observation's alike
/// (ADR-0026). Guarded like [`record_transitions`].
fn record_cursor_moves(rule: &RuleDefinition, before: &Call, after: &Call, now_ms: i64) {
    if !crate::trace::sampled(after) {
        return;
    }
    for (machine, to) in &after.sm_cursors {
        if before.sm_cursors.get(machine) != Some(to) {
            let from = before.sm_cursors.get(machine).map(call::StateLabel::as_str).unwrap_or("");
            crate::trace::emit::rule_transition(
                after,
                now_ms,
                rule.id,
                machine.as_str(),
                from,
                to.as_str(),
            );
        }
    }
    // A removed cursor is machine deactivation (`ClearState`, ADR-0016 X9).
    for (machine, from) in &before.sm_cursors {
        if !after.sm_cursors.contains_key(machine) {
            crate::trace::emit::rule_transition(
                after,
                now_ms,
                rule.id,
                machine.as_str(),
                from.as_str(),
                "terminal",
            );
        }
    }
}

/// Assert any cursor move the winning rule caused on its **own** machine is a
/// declared `(from, to)` edge (ADR-0016 X1). Keeps the generated diagram
/// exhaustive and catches authoring bugs. Debug builds panic; release builds log
/// and proceed — an undeclared transition must never panic a live worker.
/// `torn_down`: the turn moved the call into `Terminating`/`Terminated`, so a
/// cursor that vanished is the engine's deactivation of every machine, not a
/// move of the rule's own.
fn check_declared_transition(
    rule: &RuleDefinition,
    before: &std::collections::BTreeMap<call::MachineId, call::StateLabel>,
    after: &std::collections::BTreeMap<call::MachineId, call::StateLabel>,
    torn_down: bool,
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
    // gate), so `from` is always present here. A removed cursor (`to` absent) is
    // machine **deactivation** (`ClearState`) — legal iff the rule declared a
    // transition to the `terminal` sentinel from `f` (ADR-0016 X9) — or the
    // engine's, when this turn began the teardown.
    let declared = match (from, to) {
        (Some(f), Some(t)) => rule.transitions.iter().any(|(df, dt)| df == f && dt == t),
        (Some(_), None) if torn_down => true,
        (Some(f), None) => rule.transitions.iter().any(|(df, dt)| df == f && dt.is_terminal()),
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
            tracing::warn!(
                rule = %rule.id,
                machine = machine.as_str(),
                from = ?from.map(call::StateLabel::as_str),
                to = ?to.map(call::StateLabel::as_str),
                "rule caused an undeclared transition"
            );
        }
    }
}

/// Assert every **tracked** side effect a machine-bound rule emits was declared
/// in its `effects` (ADR-0016 X9). The check is **by category** ([`EffectKind`]),
/// so a handler targeting a dynamically-named leg still satisfies a `LegMessage`
/// declaration; cursor moves (`SetState`/`ClearState`) and bookkeeping are
/// auto-allowed (`EffectKind::is_tracked` is false). Core (machine-less) rules
/// declare no effects and are not checked. Debug builds panic (a test failure);
/// release builds log and proceed — an authoring gap must never panic a live
/// worker. Mirrors [`check_declared_transition`].
fn check_declared_effects(rule: &RuleDefinition, emitted: &[RuleAction]) {
    if rule.machine.is_none() {
        return;
    }
    let declared: HashSet<EffectKind> = rule.effects.iter().map(|e| e.kind()).collect();
    for action in emitted {
        let kind = action.effect_kind();
        if kind.is_tracked() && !declared.contains(&kind) {
            if cfg!(debug_assertions) {
                panic!(
                    "rule '{}' emitted an undeclared {:?} side effect \
                     (declare it in the rule's `effects`)",
                    rule.id, kind,
                );
            } else {
                tracing::warn!(rule = %rule.id, effect = ?kind, "rule emitted an undeclared side effect");
            }
        }
    }
}
