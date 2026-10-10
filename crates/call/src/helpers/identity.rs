//! The tags an in-dialog request carries ([`RequestTags`]), and the question
//! the dialog they name answers on a confirmed callee leg.

use crate::model::{Call, LegState};

use super::leg::find_b_leg;

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
}

/// Whether a request from confirmed b-leg `leg_id`'s peer names no live
/// dialog of the leg: an early dialog the answer retired, or none at all
/// (RFC 3261 §12.2.2).
pub fn abandoned_dialog(call: &Call, leg_id: &str, tags: RequestTags<'_>) -> bool {
    find_b_leg(call, leg_id).filter(|leg| leg.state == LegState::Confirmed).is_some_and(|leg| {
        tags.from.is_some()
            && !leg.dialogs.of_request(tags.to, tags.from).is_some_and(|d| d.is_live())
    })
}
