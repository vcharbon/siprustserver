//! The session description seam: every description this stack puts on a leg
//! passes here, with the leg its author spoke on, so each dialog carries ONE
//! session whoever authored what crosses it (RFC 3264 §8).
//!
//! A dialog carries the session of the author whose description opened it.
//! That author's own later descriptions leave as written while the stack has
//! stated no version of its own in that session, so a plain relay stays
//! byte-transparent. Once the leg is confirmed (its dialog answered), any
//! other description is
//! restated under the session the dialog carries
//! ([`sip_message::restate_session`]): another leg's (a transfer target, a
//! rerouted destination, a media server), one of the stack's own, the
//! author's under a new sess-id, and the author's own once the stack has
//! restated. The same author's same version again (a repeated provisional,
//! the final repeating it) is the same description: restated at the version
//! already given. What the peer then describes travels back to that author in
//! the author's stream order, without the slots the author never described
//! ([`sip_message::in_author_order`]). Before confirmation (the initial
//! INVITE, its provisionals and final) a description opens the session.

use std::borrow::Cow;

use call::{Call, Leg, LegSdpSession, LegState};

use crate::effects::{OutboundBody, OutboundSipEffect};
use sip_message::header::MediaType;
use sip_message::multipart::sdp_range;
use sip_message::{
    in_author_order, parse_origin, restate_session, restate_session_again, Method, StatedSession,
};

/// Who wrote a description the stack sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author<'a> {
    /// The peer of this leg.
    Leg(&'a str),
    /// This stack.
    Stack,
}

impl<'a> From<&'a b2bua_sdk::model::BodyAuthor> for Author<'a> {
    fn from(author: &'a b2bua_sdk::model::BodyAuthor) -> Self {
        match author {
            b2bua_sdk::model::BodyAuthor::Stack => Self::Stack,
            b2bua_sdk::model::BodyAuthor::Leg(leg) => Self::Leg(leg),
        }
    }
}

/// Where a description stands in the offer/answer exchange of its dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carried {
    /// In the initial INVITE or its provisionals and final: it opens the
    /// session.
    Opening,
    /// In a later request of the dialog, or the initial ACK.
    InDialog,
    /// In a response of the dialog to the leg's peer: the same author's same
    /// version answering no newer offer of that peer (a repeated provisional,
    /// the final repeating the answer a reliable provisional or a nested
    /// UPDATE/PRACK exchange already gave) is the same description.
    Answering,
    /// Outside any exchange (RFC 3264 §4 / RFC 3262 §5 / RFC 3311 §5 name
    /// INVITE, ACK, PRACK and UPDATE; a response of 300 or more describes
    /// capabilities, RFC 3261 §13.2.1 / §21.4.26): left as written, recorded
    /// nowhere.
    Outside,
}

impl Carried {
    /// A description in a `method` request, or in a response of `status` to
    /// one, `within_dialog` or in the initial INVITE's exchange.
    pub fn of(method: &Method, status: Option<u16>, within_dialog: bool) -> Self {
        let negotiates =
            matches!(method, Method::Invite | Method::Ack | Method::Prack | Method::Update);
        match (negotiates, status) {
            (false, _) => Self::Outside,
            (true, Some(s)) if s >= 300 => Self::Outside,
            (true, _) if within_dialog => Self::InDialog,
            (true, _) => Self::Opening,
        }
    }

    /// A description in a response of `status` to a `method` request of the
    /// dialog.
    pub fn answering(method: &Method, status: u16) -> Self {
        match Self::of(method, Some(status), true) {
            Self::InDialog => Self::Answering,
            other => other,
        }
    }

    fn in_dialog(self) -> bool {
        matches!(self, Self::InDialog | Self::Answering)
    }
}

/// `body`, typed `content_type` and written by `author`, as it leaves on
/// `leg_id` where it stands `carried`, with the leg's session state updated to
/// what it says. A body that carries no session description leaves as it is.
/// A version at the top of its range has no next one: a description that would
/// be restated above it leaves as written, and opens the session anew where the
/// stack had stated no version of its own (RFC 3264 §8 — never a wrapped one).
pub fn continue_on_leg(
    call: &mut Call,
    leg_id: &str,
    author: Author<'_>,
    carried: Carried,
    body: Vec<u8>,
    content_type: Option<&MediaType>,
) -> Vec<u8> {
    if carried == Carried::Outside {
        return body;
    }
    let Some(range) = content_type.and_then(|ct| sdp_range(ct, &body)) else {
        return body;
    };
    let sdp = &body[range.clone()];
    let Some(session_id) = parse_origin(sdp).map(|o| o.session_id) else {
        return body;
    };
    let reordered = match author {
        Author::Leg(from) if carried.in_dialog() => {
            in_restated_author_order(call, from, leg_id, sdp)
        }
        _ => None,
    };
    let sdp = reordered.as_deref().unwrap_or(sdp);
    let Some(leg) = leg_mut(call, leg_id) else {
        return body;
    };
    let repeat_key = match carried {
        Carried::Answering => {
            parse_origin(sdp).map(|o| format!("{} {}", leg.sdp_session.offers_received, o.value()))
        }
        _ => None,
    };
    let restated = (carried.in_dialog()
        && leg.state == LegState::Confirmed
        && !continues_itself(&leg.sdp_session, author, &session_id))
    .then(|| stated(leg))
    .flatten()
    .and_then(|stated| {
        if repeats(&leg.sdp_session, author, repeat_key.as_deref()) {
            restate_session_again(sdp, &stated)
        } else {
            restate_session(sdp, &stated)
        }
    });
    let state = &mut leg.sdp_session;
    let out = match restated {
        Some(r) => {
            state.sent_slots = r.slots;
            state.sent_slots_author = match author {
                Author::Leg(from) => Some(from.to_string()),
                Author::Stack => None,
            };
            state.restated_from = repeat_key;
            state.restated = true;
            Cow::Owned(r.sdp)
        }
        None => {
            state.sent_slots = Vec::new();
            state.sent_slots_author = None;
            state.restated_from = None;
            if carried == Carried::Opening || state.sent_origin.is_none() || !state.restated {
                state.session_author = match author {
                    Author::Leg(from) => Some(from.to_string()),
                    Author::Stack => None,
                };
                state.session_id = Some(session_id);
                state.restated = false;
            }
            Cow::Borrowed(sdp)
        }
    };
    if let Some(now) = StatedSession::of(&out) {
        state.sent_origin = Some(now.origin);
        state.sent_media = now.media;
    }
    if matches!(out, Cow::Borrowed(_)) && reordered.is_none() {
        return body;
    }
    [&body[..range.start], &out, &body[range.end..]].concat()
}

/// Count an offer/answer exchange `leg_id`'s peer opens: a `method` request
/// carrying a session description in `body` (typed `content_type`).
pub fn note_request(
    call: &mut Call,
    leg_id: &str,
    method: &Method,
    body: &[u8],
    content_type: Option<&MediaType>,
) {
    let opens = matches!(method, Method::Invite | Method::Update | Method::Prack)
        && content_type.and_then(|ct| sdp_range(ct, body)).is_some();
    if let (true, Some(leg)) = (opens, leg_mut(call, leg_id)) {
        leg.sdp_session.offers_received = leg.sdp_session.offers_received.saturating_add(1);
    }
}

/// The session state of a leg this stack opens with `invite`, its initial
/// INVITE, whose description `author` wrote.
pub fn opened(invite: &OutboundSipEffect, author: Author<'_>) -> LegSdpSession {
    let OutboundBody::Request(req) = &invite.body else {
        return LegSdpSession::default();
    };
    let Some(sdp) = req.sdp() else {
        return LegSdpSession::default();
    };
    let (Some(stated), Some(origin)) = (StatedSession::of(sdp), parse_origin(sdp)) else {
        return LegSdpSession::default();
    };
    LegSdpSession {
        sent_origin: Some(stated.origin),
        sent_media: stated.media,
        session_author: match author {
            Author::Leg(from) => Some(from.to_string()),
            Author::Stack => None,
        },
        session_id: Some(origin.session_id),
        ..LegSdpSession::default()
    }
}

/// Whether a description by `author` stating `session_id` continues, by its
/// author's own account, the session `state`'s dialog carries: the dialog's
/// author, the same sess-id, and no version of the stack's own in between.
fn continues_itself(state: &LegSdpSession, author: Author<'_>, session_id: &str) -> bool {
    match author {
        Author::Leg(from) => {
            state.session_author.as_deref() == Some(from)
                && state.session_id.as_deref() == Some(session_id)
                && !state.restated
        }
        Author::Stack => false,
    }
}

/// `sdp`, written by the peer of `from` and going to `to`, in the stream order
/// of `to`'s description the stack last restated toward `from`, or `None`
/// where that description was not `to`'s (or not restated at all).
fn in_restated_author_order(call: &Call, from: &str, to: &str, sdp: &[u8]) -> Option<Vec<u8>> {
    let state =
        &std::iter::once(&call.a_leg).chain(&call.b_legs).find(|l| l.leg_id == from)?.sdp_session;
    (state.sent_slots_author.as_deref() == Some(to) && !state.sent_slots.is_empty())
        .then(|| in_author_order(sdp, &state.sent_slots))
        .flatten()
}

/// Whether a description by `author` under `key` (the count of the peer's
/// offers it answers and its `o=` value) repeats the one the stack last
/// restated on the leg.
fn repeats(state: &LegSdpSession, author: Author<'_>, key: Option<&str>) -> bool {
    let Author::Leg(from) = author else { return false };
    state.sent_slots_author.as_deref() == Some(from)
        && key.is_some()
        && state.restated_from.as_deref() == key
}

/// What the stack last stated on `leg`, as the next restatement continues it.
fn stated(leg: &Leg) -> Option<StatedSession> {
    Some(StatedSession {
        origin: leg.sdp_session.sent_origin.clone()?,
        media: leg.sdp_session.sent_media.clone(),
    })
}

fn leg_mut<'a>(call: &'a mut Call, leg_id: &str) -> Option<&'a mut Leg> {
    std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()).find(|l| l.leg_id == leg_id)
}

#[cfg(test)]
#[path = "sdp_session_tests.rs"]
mod tests;
