//! The caller's INVITE of a call, rebuilt from its record by the step that
//! reads it (ADR-0052 §11).

use call::Call;
use sip_message::SipRequest;

use crate::turn_notes::{TurnNotes, CALLER_INVITE_REBUILDS};

pub use b2bua_sdk::caller_invite::rebuild as rebuild_a_leg_invite;

/// The caller's INVITE of `call`, rebuilt from its record. Every call is
/// checked to rebuild it where it becomes resident, so a record that does not
/// is a defect: noted on `notes`, and the turn's call is unrecoverable
/// ([`TurnNotes::unrecoverable`]).
pub fn caller_invite(call: &Call, notes: &mut TurnNotes) -> Option<SipRequest> {
    match rebuild_a_leg_invite(&call.a_leg_invite) {
        Ok(invite) => Some(invite),
        Err(err) => {
            note_unrebuilt(call, notes, &err);
            None
        }
    }
}

/// Note on `notes` that `call`'s record does not rebuild its caller's INVITE.
pub fn note_unrebuilt(call: &Call, notes: &mut TurnNotes, why: &dyn std::fmt::Display) {
    notes.defect(CALLER_INVITE_REBUILDS, format!("the caller's INVITE: {why}"), call);
}
