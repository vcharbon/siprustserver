//! The two base lenses every other helper builds on: apply a closure to one
//! leg, or to the dialogs matching a leg-appropriate identity tag. Helpers take
//! the [`Call`] by value, mutate in place, and return it — value semantics from
//! the caller's view, without deep clones.

use crate::model::{Call, Dialog, Leg};

/// Dialog-identity match — a-leg keys off `localTag`, b-leg off `remoteTag`.
pub(super) fn match_dialog_identity(leg_id: &str, identity_tag: &str, d: &Dialog) -> bool {
    if leg_id == "a" {
        d.sip.local_tag == identity_tag
    } else {
        d.sip.remote_tag == identity_tag
    }
}

/// Apply `f` to the leg with `leg_id` (a-leg or any matching b-leg).
pub fn update_leg(mut call: Call, leg_id: &str, f: impl FnOnce(&mut Leg)) -> Call {
    if call.a_leg.leg_id == leg_id {
        f(&mut call.a_leg);
    } else if let Some(l) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        f(l);
    }
    call
}

/// Apply `f` to every dialog within `leg_id` matching `identity_tag`.
pub fn update_dialog(
    call: Call,
    leg_id: &str,
    identity_tag: &str,
    mut f: impl FnMut(&mut Dialog),
) -> Call {
    update_leg(call, leg_id, |leg| {
        for d in &mut leg.dialogs {
            if match_dialog_identity(leg_id, identity_tag, d) {
                f(d);
            }
        }
    })
}
