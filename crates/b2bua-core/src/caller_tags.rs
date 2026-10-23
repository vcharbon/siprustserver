//! The To-tags this stack mints toward the caller, each derived from the
//! call's [`tag_seed`](call::Call::tag_seed) and what the tag fronts (RFC 3261
//! §19.3): any owner of the call derives the tag its first owner showed the
//! caller for the same dialog, so a final, a BYE or an ACK it reads names
//! that dialog alike on every node (ADR-0055 §4).

use call::Call;

/// The caller dialog of the a-leg itself: the tag of a response no callee
/// dialog names, and the one the first callee dialog shown is fronted under.
pub fn own(call: &Call) -> String {
    sip_ids::derived_tag(call.tag_seed, &[call.a_leg.leg_id.as_bytes()])
}

/// The caller dialog fronting the callee dialog `b_tag` of the leg `b_leg`.
pub fn fronting(call: &Call, b_leg: &str, b_tag: &str) -> String {
    sip_ids::derived_tag(call.tag_seed, &[b_leg.as_bytes(), b_tag.as_bytes()])
}

/// The caller dialog the early media of the leg `leg` is shown under, or the
/// one a 2xx opens beside the caller's early dialogs on the leg's behalf.
pub fn of_leg(call: &Call, leg: &str) -> String {
    sip_ids::derived_tag(call.tag_seed, &[leg.as_bytes(), b"leg"])
}

/// The caller dialog a 2xx opens in place of the early dialog the caller was
/// shown media under ([`b2bua_sdk::model::RuleAction::AnswerALegNewDialog`]).
pub fn answering(call: &Call) -> String {
    sip_ids::derived_tag(call.tag_seed, &[call.a_leg.leg_id.as_bytes(), b"answering"])
}
