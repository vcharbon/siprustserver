//! Where a call stands on the two monotone axes of its life, and how two copies
//! of one call rank on them. Two copies that diverged end the call from the one
//! ranked higher (ADR-0055 §7).

use crate::model::{Call, CallModelState};

/// The call's position as `(an answer was recorded for the caller, active <
/// terminating < terminated)`. Both components are monotone over one owner's
/// copy: neither ever moves backward. "Answered" is read from the durable
/// record of the final the a-leg's initial INVITE server transaction took, so
/// it stays true once the leg is `Terminated`, and from the answer a CANCEL
/// pre-empted ([`crate::Leg::answer_preempted`]), so it stays true when the
/// transaction layer's 487 overtook a recorded 2xx.
pub fn lifecycle_position(call: &Call) -> (u8, u8) {
    let ending = match call.state {
        CallModelState::Active => 0,
        CallModelState::Terminating => 1,
        CallModelState::Terminated => 2,
    };
    let answered = super::caller_answered(call) || call.a_leg.answer_preempted().is_some();
    (u8::from(answered), ending)
}

/// Does `incoming` rank behind `held` on the call's lifecycle? The answer
/// axis ranks first: **an unanswered teardown never ends an answered call**,
/// so an answered body is never behind an unanswered one, however far toward
/// its end the unanswered one went, and an unanswered body is always behind an
/// answered one. Between two bodies alike on the answer, the one less far
/// toward its end is behind.
pub fn lifecycle_behind(incoming: &Call, held: &Call) -> bool {
    lifecycle_position(incoming) < lifecycle_position(held)
}
