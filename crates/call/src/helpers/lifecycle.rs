//! Where a call stands on the two monotone axes of its life, and how two copies
//! of one call compare on them. Replication reconciliation is the caller: a
//! version vector says which copy is newer, these say which copy is *further
//! along the same call* (ADR-0031 D3).

use crate::model::{Call, CallModelState};

/// The call's position as `(the caller was answered, active < terminating <
/// terminated)`. Both components are monotone over one owner's copy: neither
/// ever moves backward, and "answered" is read from the durable record of the
/// final the a-leg's initial INVITE server transaction took, so it stays true
/// once the leg is `Terminated`.
pub fn lifecycle_position(call: &Call) -> (u8, u8) {
    let ending = match call.state {
        CallModelState::Active => 0,
        CallModelState::Terminating => 1,
        CallModelState::Terminated => 2,
    };
    (u8::from(super::caller_answered(call)), ending)
}

/// Does `incoming` sit BEHIND `held` on either axis? Then it is a branch off a
/// view its owner left behind — a pre-answer teardown of a call the other owner
/// answered, or a live body for a call the other owner is ending — never a newer
/// version of the same call.
pub fn lifecycle_regresses(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i.0 < h.0 || i.1 < h.1
}

/// Does `incoming` carry progress `held` lacks — every step `held` made on BOTH
/// axes, and at least one more? Progress is a chain, not a rank: a terminal body
/// whose caller was never answered does not outrank a live answered one.
pub fn lifecycle_advances(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    !lifecycle_regresses(incoming, held) && i != h
}
