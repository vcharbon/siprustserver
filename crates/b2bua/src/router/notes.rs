//! A turn's notes drained (ADR-0052 §4): as each core call returns, before
//! the turn records or sends anything else.

use super::RouterCtx;
use crate::turn_scope::TurnScope;

/// Drain what `turn` has reported on `call_ref`. The direct emission path
/// has already traced and counted all of it, so the notes go.
pub(super) fn drain(ctx: &RouterCtx, call_ref: &str, turn: &mut TurnScope) {
    let _ = (ctx, call_ref, turn.notes.take());
}
