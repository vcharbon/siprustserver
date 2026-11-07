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
    for defect in notes.defects() {
        crate::model_invariants::defect(&ctx.metrics, defect);
    }
}
