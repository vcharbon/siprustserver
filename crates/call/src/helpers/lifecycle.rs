//! Where a call stands on the two monotone axes of its life, and how two copies
//! of one call compare on them and on the re-INVITE rounds they share.
//! Replication reconciliation is the caller: a version vector says which copy
//! is newer, these say which copy is *further along the same call* (ADR-0031
//! D3).

use crate::model::{Call, CallModelState};

use super::round::round_further;

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
    let answered = super::caller_answered(call) || call.a_leg.answer_preempted.is_some();
    (u8::from(answered), ending)
}

/// Does `incoming` sit BEHIND `held` on either axis, or on a re-INVITE round
/// ([`round_further`])? Then it is a branch off a view its owner left behind —
/// a pre-answer teardown of a call the other owner answered, a live body for a
/// call the other owner is ending, a round still unfinished that the other
/// owner saw end — never a newer version of the same call.
pub fn lifecycle_regresses(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i.0 < h.0 || i.1 < h.1 || round_further(held, incoming)
}

/// Does `incoming` stand behind `held` as a version of the call one owner
/// would keep over the other? The answer axis ranks first: **an unanswered
/// teardown never ends an answered call**, so an answered body is never
/// behind an unanswered one, however far toward its end the unanswered one
/// went, and an unanswered body is always behind an answered one. Between two
/// bodies alike on the answer, the one less far toward its end is behind;
/// between two alike on both, the one holding unfinished a re-INVITE round the
/// other saw end ([`round_further`]). The replication gates read it both ways:
/// a reverse flush behind the primary's live copy never folds as the call's
/// newer version, and a forward flush behind the backup's Element is a branch
/// the Element refuses (ADR-0031 D3).
pub fn lifecycle_behind(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i < h || (i == h && round_further(held, incoming))
}

/// What `incoming` carries that `held` lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A further step on the answer or the ending axis: the body is the call
    /// further along.
    Lifecycle,
    /// Only the end of a re-INVITE round `held` holds unfinished
    /// ([`super::round::merge_rounds`] takes it).
    Round,
}

/// The progress `incoming` carries over `held` — every step `held` made on
/// BOTH axes and on every round they share, and at least one more — or `None`.
/// Progress is a chain, not a rank: a terminal body whose caller was never
/// answered does not outrank a live answered one.
pub fn lifecycle_progress(incoming: &Call, held: &Call) -> Option<Progress> {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    if lifecycle_regresses(incoming, held) {
        None
    } else if i != h {
        Some(Progress::Lifecycle)
    } else {
        round_further(incoming, held).then_some(Progress::Round)
    }
}

/// Whether `incoming` carries any progress over `held` ([`lifecycle_progress`]).
pub fn lifecycle_advances(incoming: &Call, held: &Call) -> bool {
    lifecycle_progress(incoming, held).is_some()
}
