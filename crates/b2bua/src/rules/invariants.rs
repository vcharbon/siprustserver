//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten (cancel-all-timers, write-cdr,
//! remove-call), so termination is always clean. Termination is also *promoted*
//! (`terminating → terminated`) once every leg is resolved.

use std::collections::HashSet;

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState};

use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, HandlerResult, SoftBoundedEffect,
};

/// Promote `terminating → terminated` when all legs are resolved.
pub fn finalize(mut result: HandlerResult) -> HandlerResult {
    if result.call.state == CallModelState::Terminating && is_fully_resolved(&result.call) {
        result.call.state = CallModelState::Terminated;
    }
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
