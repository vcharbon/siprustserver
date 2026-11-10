//! A turn's notes drained (ADR-0052 §4): as each core call returns, before
//! the turn records or sends anything else, its trace facts reach the call's
//! trace, its counts the metrics, and its defects the model oracle
//! (ADR-0052 §8), in the order the turn met them.

use super::RouterCtx;
use crate::turn_scope::TurnScope;

/// Drain what `turn` has reported on `call_ref`.
pub(super) fn drain(ctx: &RouterCtx, call_ref: &str, turn: &mut TurnScope) {
    let notes = turn.notes.take();
    for fact in notes.trace_facts() {
        crate::trace::emit::turn_fact(call_ref, fact);
    }
    for kind in notes.repeat_give_ups() {
        ctx.metrics.record_repeat_give_up(kind);
    }
    for _ in 0..notes.message_caps() {
        ctx.metrics.bump_message_cap_terminated();
    }
    for cap in notes.caps_near() {
        ctx.metrics.record_cap_near(*cap);
    }
    for (cap, value, per_hour) in notes.cap_readings().iter() {
        ctx.metrics.record_cap_reading(cap, value, per_hour);
    }
    for defect in notes.defects() {
        crate::model_invariants::defect(&ctx.metrics, defect);
    }
}

/// Drain what a turn past a per-call cap reported (ADR-0057 §4): its result
/// is discarded, so only the defects it met stand; its trace facts and counts
/// name what never happened.
pub(super) fn drain_discarded(ctx: &RouterCtx, turn: &mut TurnScope) {
    for defect in turn.notes.take().defects() {
        crate::model_invariants::defect(&ctx.metrics, defect);
    }
}
