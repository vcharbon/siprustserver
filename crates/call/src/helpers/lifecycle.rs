//! Where a call stands on the two monotone axes of its life, and how two copies
//! of one call compare on them and on the re-INVITE rounds they share.
//! Replication reconciliation is the caller: a version vector says which copy
//! is newer, these say which copy is *further along the same call* (ADR-0031
//! D3).

use crate::model::{Call, CallModelState};

use super::round::round_ahead;

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

/// Does `incoming` stand behind `held` as a version of the call one owner
/// would keep over the other? The answer axis ranks first: **an unanswered
/// teardown never ends an answered call**, so an answered body is never
/// behind an unanswered one, however far toward its end the unanswered one
/// went, and an unanswered body is always behind an answered one. Between two
/// bodies alike on the answer, the one less far toward its end is behind;
/// between two alike on both, the one lacking a re-INVITE round's progress the
/// other holds — its end or its opening ([`round_ahead`]). The replication
/// gates read it both ways: a reverse flush behind the primary's live copy
/// never folds as the call's newer version, and a forward flush behind the
/// backup's Element is a branch the Element refuses (ADR-0031 D3).
pub fn lifecycle_behind(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i < h || (i == h && round_ahead(held, incoming))
}

/// What `incoming` carries that `held` lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// A further step on the answer or the ending axis: the body is the call
    /// further along.
    Lifecycle,
    /// Only re-INVITE rounds: the end of one `held` holds unfinished, or the
    /// opening of one it never reached ([`super::round::merge_rounds`] takes
    /// them, dialog by dialog).
    Round,
}

/// The progress `incoming` carries over `held`, or `None`. On the answer and
/// the ending axes progress is a chain, not a rank: `incoming` made every
/// step `held` made on both, so a terminal body whose caller was never
/// answered does not outrank a live answered one, and a step further on
/// either is the call further along, whatever round `held` holds — the
/// rounds `held` stands ahead on stay its own when the body folds
/// ([`super::round::merge_rounds`] from `held`). Alike on both axes, a round
/// `incoming` stands ahead on, on any dialog, is progress of its own: each
/// dialog's round has one client transaction, and the rounds `held` stands
/// ahead on, on other dialogs, stay its own when the rounds fold.
pub fn lifecycle_progress(incoming: &Call, held: &Call) -> Option<Progress> {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    if i.0 < h.0 || i.1 < h.1 {
        None
    } else if i != h {
        Some(Progress::Lifecycle)
    } else {
        round_ahead(incoming, held).then_some(Progress::Round)
    }
}

/// Whether `incoming` carries any progress over `held` ([`lifecycle_progress`]).
pub fn lifecycle_advances(incoming: &Call, held: &Call) -> bool {
    lifecycle_progress(incoming, held).is_some()
}
