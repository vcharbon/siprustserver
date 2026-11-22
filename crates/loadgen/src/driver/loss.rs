//! A call's loss and departure taps: what the loss model's drops, and the
//! datagrams that left despite it, tell the call.

use std::sync::Arc;

use scenario_harness::dialogs::Departures;

use crate::ctx::CallCtx;
use crate::outcome::CallWire;

/// The call's loss tap: each drop noted on `ctx`, and told to the call's
/// wire facts when it has them.
pub(super) fn loss_tap(ctx: &CallCtx, wire: Option<&CallWire>) -> crate::mux::LossTap {
    let notes = crate::mux::notes_tap(ctx.loss_notes());
    let Some(wire) = wire.map(CallWire::loss_tap) else { return notes };
    Arc::new(move |side, dir, d, arrived_before| {
        notes(side, dir, d, arrived_before);
        wire(side, dir, d, arrived_before);
    })
}

/// The call's departure tap: each datagram that left noted in `log`, which
/// its agents' dialog judges read.
pub(super) fn departure_tap(log: &Departures) -> crate::mux::DepartureTap {
    let log = log.clone();
    Arc::new(move |id| log.note(id))
}
