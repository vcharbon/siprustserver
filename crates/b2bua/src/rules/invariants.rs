//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten (cancel-all-timers, write-cdr,
//! remove-call), so termination is always clean. Termination is also *promoted*
//! (`terminating → terminated`) once every leg is resolved.

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState};

use crate::effects::{BufferedObservabilityEffect, CriticalStateEffect, HandlerResult};

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

    // remove-call must run last.
    result
        .effects
        .critical
        .retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}
