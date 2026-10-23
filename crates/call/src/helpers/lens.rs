//! The two base lenses every other helper builds on: apply a closure to one
//! leg, or to one dialog of a leg named by its tags. Helpers take the
//! [`Call`] by value, mutate in place, and return it — value semantics from
//! the caller's view, without deep clones.

use crate::model::{Call, Dialog, DialogId, Leg};

/// Apply `f` to the leg with `leg_id` (a-leg or any matching b-leg).
pub fn update_leg(mut call: Call, leg_id: &str, f: impl FnOnce(&mut Leg)) -> Call {
    if call.a_leg.leg_id == leg_id {
        f(&mut call.a_leg);
    } else if let Some(l) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        f(l);
    }
    call
}

/// Apply `f` to the dialog of `leg_id` that `id` names; a miss changes nothing.
pub fn update_dialog(call: Call, leg_id: &str, id: &DialogId, f: impl FnOnce(&mut Dialog)) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag) {
            f(d);
        }
    })
}
