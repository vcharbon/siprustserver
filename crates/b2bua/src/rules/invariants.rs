//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten (cancel-all-timers, write-cdr,
//! remove-call), so termination is always clean. Termination is also *promoted*
//! (`terminating → terminated`) once every leg is resolved.

use std::collections::HashSet;

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState, MachineId, StateLabel};

use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, HandlerResult, SoftBoundedEffect,
};

/// The always-on global call machine (ADR-0016 X2). Its cursor is a uniform,
/// read-only **projection** of the authoritative `CallModelState` — the engine,
/// the doc generator, observability, and HA reconciliation read every machine's
/// position through `sm_cursors` regardless of crate. The `state` field stays the
/// single source of truth (this is the one cursor `SetState` does not write).
pub const GLOBAL_CALL_MACHINE: MachineId = MachineId::new("global-call");

fn global_call_label(state: CallModelState) -> StateLabel {
    match state {
        CallModelState::Active => StateLabel::new("Active"),
        CallModelState::Terminating => StateLabel::new("Terminating"),
        CallModelState::Terminated => StateLabel::new("Terminated"),
    }
}

/// Promote `terminating → terminated` when all legs are resolved, then project
/// the (possibly promoted) `CallModelState` into the `global-call` cursor. This
/// is the single finalize point, so the global machine is observable uniformly
/// without touching the authoritative `state` field or the termination logic.
pub fn finalize(mut result: HandlerResult) -> HandlerResult {
    if result.call.state == CallModelState::Terminating && is_fully_resolved(&result.call) {
        result.call.state = CallModelState::Terminated;
    }
    result
        .call
        .sm_cursors
        .insert(GLOBAL_CALL_MACHINE, global_call_label(result.call.state));
    result
}

/// Guarantee cleanup on the `→ terminated` transition.
pub fn enforce(before: &Call, mut result: HandlerResult) -> HandlerResult {
    let became_terminated =
        before.state != CallModelState::Terminated && result.call.state == CallModelState::Terminated;
    if !became_terminated {
        return result;
    }
    let crit = &mut result.effects.critical;
    if !crit.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)) {
        crit.insert(0, CriticalStateEffect::CancelAllTimers);
    }
    result.call.timers.clear();

    if !result
        .effects
        .buffered
        .iter()
        .any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
    {
        result.effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
    }

    // Limiter release: every recorded hold is decremented exactly once on
    // termination (the strong INCR↔DECR invariant). Fail-open admissions
    // (`increment_succeeded == Some(false)`) carry no real increment, so they
    // are skipped. Dedupe against any release a rule already emitted.
    let already: HashSet<(String, i64)> = result
        .effects
        .soft
        .iter()
        .map(|SoftBoundedEffect::DecrementLimiter { limiter_id, window }| {
            (limiter_id.clone(), *window)
        })
        .collect();
    for entry in &result.call.limiter_entries {
        if entry.increment_succeeded == Some(false) {
            continue;
        }
        let key = (entry.limiter_id.clone(), entry.origin_window);
        if already.contains(&key) {
            continue;
        }
        result.effects.soft.push(SoftBoundedEffect::DecrementLimiter {
            limiter_id: entry.limiter_id.clone(),
            window: entry.origin_window,
        });
    }

    // remove-call must run last.
    result
        .effects
        .critical
        .retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}
