//! Which dialog of a leg an in-dialog message names. The a-leg keeps one
//! dialog per caller-facing tag, keyed by the B2BUA's own tag; a b-leg keeps
//! one per callee fork, keyed by the peer's tag (RFC 3261 §12.1.2).

use crate::model::{Call, Dialog, Leg};

use super::leg::{confirmed_dialog, find_leg};
use super::lens::match_dialog_identity;

/// The To-tag and From-tag an in-dialog request carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestTags<'a> {
    pub to: Option<&'a str>,
    pub from: Option<&'a str>,
}

impl<'a> RequestTags<'a> {
    pub fn new(to: Option<&'a str>, from: Option<&'a str>) -> Self {
        RequestTags { to, from }
    }

    /// The identity tag of the dialog a request from `leg_id`'s peer rides:
    /// its To-tag on the a-leg (the B2BUA's tag), its From-tag on a b-leg.
    pub fn identity(&self, leg_id: &str) -> Option<&'a str> {
        if leg_id == "a" {
            self.to
        } else {
            self.from
        }
    }
}

/// The identity tag of the dialog a response arriving on `leg_id` belongs to:
/// its From-tag on the a-leg (the B2BUA's tag), its To-tag on a b-leg.
pub fn response_identity<'a>(
    leg_id: &str,
    to: Option<&'a str>,
    from: Option<&'a str>,
) -> Option<&'a str> {
    if leg_id == "a" {
        from
    } else {
        to
    }
}

/// The dialog of `leg` whose identity tag is `tag`.
pub fn dialog_by_identity<'a>(leg: &'a Leg, tag: &str) -> Option<&'a Dialog> {
    leg.dialogs.iter().find(|d| match_dialog_identity(&leg.leg_id, tag, d))
}

/// The dialog a request from `leg_id`'s peer rides: the one its tags name,
/// else the leg's confirmed dialog.
pub fn request_dialog<'a>(
    call: &'a Call,
    leg_id: &str,
    tags: RequestTags<'_>,
) -> Option<&'a Dialog> {
    let leg = find_leg(call, leg_id)?;
    tags.identity(leg_id).and_then(|t| dialog_by_identity(leg, t)).or_else(|| confirmed_dialog(leg))
}
