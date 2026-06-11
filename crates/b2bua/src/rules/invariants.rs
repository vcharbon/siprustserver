//! Framework guarantees — port of `InvariantEnforcer.ts` + the bye-disposition
//! invariant. On the `→ terminated` transition the framework appends the
//! cleanup a buggy rule might have forgotten: cancel-all-timers first, then
//! every **obligation** the call still owes (the CDR, the limiter decrements —
//! derived from the snapshot by the [`ObligationSet`], ADR-0020 X7), then
//! remove-call last — so termination is always clean. Termination is also
//! *promoted* (`terminating → terminated`) once every leg is resolved.

use call::helpers::is_fully_resolved;
use call::{Call, CallModelState, MachineId, StateLabel};

use crate::effects::{CriticalStateEffect, HandlerResult};
use crate::obligations::ObligationSet;

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
    // Project the authoritative `Call.transfer.phase` into the `transfer` machine
    // cursor (ADR-0016 slice 7) the same way — a read-only view the transfer
    // service rules gate on; clearing the slice removes the cursor.
    super::refer_transfer::project_cursor(&mut result.call);
    // Project `(strategy, first_relayed)` into the `relayFirst18x` machine cursor
    // (ADR-0016) — Masking → Suppressing as the first 18x is relayed; absent when
    // no masking strategy is active (incl. the delayed-offer self-disable).
    super::relay_first_18x::project_cursor(&mut result.call);
    result
}

/// Guarantee cleanup on the `→ terminated` transition: CancelAllTimers first,
/// then every owed obligation (`obligations.settle` — the CDR + limiter
/// decrements derived from the snapshot, idempotent against rule-emitted
/// cleanup), then RemoveCall last.
pub fn enforce(
    obligations: &ObligationSet,
    before: &Call,
    mut result: HandlerResult,
) -> HandlerResult {
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

    obligations.settle(&result.call, &mut result.effects);

    // remove-call must run last.
    result
        .effects
        .critical
        .retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
    result.effects.critical.push(CriticalStateEffect::RemoveCall);
    result
}
