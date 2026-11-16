//! A call's loss tap: what the loss model's drops tell the call.

use std::sync::Arc;

use crate::ctx::CallCtx;
use crate::outcome::CallWire;

/// The call's loss tap: each drop noted on `ctx`, and told to the call's
/// wire facts when it has them.
pub(super) fn loss_tap(ctx: &CallCtx, wire: Option<&CallWire>) -> crate::mux::LossTap {
    let notes = crate::mux::notes_tap(ctx.loss_notes());
    let Some(wire) = wire.map(CallWire::loss_tap) else { return notes };
    Arc::new(move |side, dir, d| {
        notes(side, dir, d);
        wire(side, dir, d);
    })
}
