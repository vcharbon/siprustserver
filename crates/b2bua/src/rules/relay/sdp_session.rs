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
//! Inside an early dialog, a description of the stack's own that already
//! states the next version of the session the dialog carries (an answer it
//! composed as that session's author would) leaves as written and counts as a
//! version the stack stated; the author's later descriptions in that dialog
//! are then restated above it as on a confirmed one. Each early dialog of a
//! leg the stack opened keeps its own session state until one of them
//! confirms (RFC 3261 §12.1.2).

use std::borrow::Cow;

use call::{Call, Leg, LegSdpSession, LegState};
use sip_message::SdpOrigin;

use crate::effects::{OutboundBody, OutboundSipEffect};
use sip_message::header::MediaType;
use sip_message::multipart::sdp_range;
use sip_message::{
    in_author_order, parse_origin, restate_session, restate_session_again, Method, SipRequest,
    StatedSession,
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
    /// version as the last restatement, with no newer offer of that peer (a
    /// repeated provisional, the final repeating what a reliable provisional
    /// or a nested UPDATE/PRACK exchange already carried), is the same
    /// description. A description in a request is always a new version.
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
/// `leg_id` inside its dialog whose remote tag is `dialog` (`None`: the leg's
/// one dialog, or none yet) where it stands `carried`, with that dialog's
/// session state updated to what it says. While `leg_id` is an unconfirmed leg
/// this stack opened, each of its early dialogs carries its own state, the
/// opening INVITE's until something crosses it; the leg keeps the opening state
/// and [`adopt_confirmed_dialog`] hands it the confirming dialog's. A body that carries no session description leaves as it is.
/// A version at the top of its range has no next one: a description that would
/// be restated above it leaves as written, and opens the session anew where the
/// stack had stated no version of its own (RFC 3264 §8 — never a wrapped one).
#[allow(clippy::too_many_arguments)]
pub fn continue_on_leg(
    call: &mut Call,
    leg_id: &str,
    dialog: Option<&str>,
    author: Author<'_>,
    carried: Carried,
    body: Vec<u8>,
    content_type: Option<&MediaType>,
) -> Vec<u8> {
    let Some(opening) = dialog.and_then(|tag| enter_early_dialog(call, leg_id, tag)) else {
        return continue_session(call, leg_id, author, carried, body, content_type);
    };
    let out = continue_session(call, leg_id, author, carried, body, content_type);
    if let (Some(tag), Some(leg)) = (dialog, call.b_legs.iter_mut().find(|l| l.leg_id == leg_id)) {
        let offers_received = leg.sdp_session.offers_received;
        let own =
            std::mem::replace(&mut leg.sdp_session, LegSdpSession { offers_received, ..opening });
        if let Some(d) = leg.dialogs.iter_mut().find(|d| d.sip.remote_tag == tag) {
            d.ext.sdp_session = Some(own);
        }
    }
    out
}

/// Puts the state early dialog `tag` of the unconfirmed leg `leg_id` carries in
/// the leg's place and returns the leg's own (the opening state), or `None`
/// where `leg_id` is not an unconfirmed leg this stack opened with such a
/// dialog.
fn enter_early_dialog(call: &mut Call, leg_id: &str, tag: &str) -> Option<LegSdpSession> {
    let leg = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id)?;
    let dialog = (leg.state != LegState::Confirmed)
        .then(|| leg.dialogs.iter_mut().position(|d| d.sip.remote_tag == tag))
        .flatten()?;
    let mut own = leg.dialogs[dialog].ext.sdp_session.take();
    if let Some(own) = own.as_mut() {
        own.offers_received = leg.sdp_session.offers_received;
    }
    Some(match own {
        Some(own) => std::mem::replace(&mut leg.sdp_session, own),
        None => leg.sdp_session.clone(),
    })
}

/// [`continue_on_leg`] on the session state in the leg's place.
fn continue_session(
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
    let repeat_key = carried
        .in_dialog()
        .then(|| parse_origin(sdp))
        .flatten()
        .map(|o| format!("{} {}", leg.sdp_session.offers_received, o.value()));
    let already_stated = author == Author::Stack
        && carried.in_dialog()
        && matches!(leg.state, LegState::Trying | LegState::Early)
        && stated(leg).is_some_and(|s| states_next_version(sdp, &s.origin));
    let restated = (carried.in_dialog()
        && match leg.state {
            LegState::Confirmed => true,
            LegState::Trying | LegState::Early => leg.sdp_session.restated,
            LegState::Terminated => false,
        }
        && !already_stated
        && !continues_itself(&leg.sdp_session, author, &session_id))
    .then(|| stated(leg))
    .flatten()
    .and_then(|stated| {
        if carried == Carried::Answering && repeats(&leg.sdp_session, author, repeat_key.as_deref())
        {
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
            if already_stated {
                state.restated = true;
            } else if carried == Carried::Opening || state.sent_origin.is_none() || !state.restated
            {
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

/// The session state the early dialog at `dialog` carries, handed to `leg` as
/// that dialog confirms it; nothing where the dialog carries the opening state.
pub fn adopt_confirmed_dialog(leg: &mut Leg, dialog: usize) {
    if let Some(mut own) = leg.dialogs.get_mut(dialog).and_then(|d| d.ext.sdp_session.take()) {
        own.offers_received = leg.sdp_session.offers_received;
        leg.sdp_session = own;
    }
}

/// The `o=` value of the next version of the session `leg`'s dialog whose
/// remote tag is `remote_tag` carries: the last one this stack stated there,
/// one up. `None` where the stack stated none there, or the version has no
/// next one.
pub fn next_origin_in_dialog(leg: &Leg, remote_tag: &str) -> Option<String> {
    let early = (leg.state != LegState::Confirmed)
        .then(|| leg.dialogs.iter().find(|d| d.sip.remote_tag == remote_tag))
        .flatten()
        .and_then(|d| d.ext.sdp_session.as_ref());
    let stated = early.unwrap_or(&leg.sdp_session).sent_origin.as_deref()?;
    let line = origin_of(stated)?.next_version_line()?;
    Some(line["o=".len()..].to_string())
}

/// Count the offer/answer exchange `req`, received from `leg_id`'s peer,
/// opens: an INVITE or UPDATE carrying a description, which is always an
/// offer (RFC 3264 §4, RFC 3311 §5.1). A PRACK's description may answer the
/// stack's own offer (RFC 3262 §5) and is not counted; a new offer it carries
/// is answered by a description that is the same, at the same version, only
/// when its author repeats it unchanged.
pub fn note_request(call: &mut Call, leg_id: &str, req: &SipRequest) {
    let opens = matches!(req.method(), Method::Invite | Method::Update) && req.sdp().is_some();
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

/// Whether `sdp` states the version after `stated` of the session `stated`
/// names: the same five identity fields, the version one up.
fn states_next_version(sdp: &[u8], stated: &str) -> bool {
    match (parse_origin(sdp), origin_of(stated)) {
        (Some(now), Some(before)) => {
            now.identifies_same_session(&before)
                && before.session_version.checked_add(1) == Some(now.session_version)
        }
        _ => false,
    }
}

/// The origin an `o=` value states.
fn origin_of(value: &str) -> Option<SdpOrigin> {
    parse_origin(format!("v=0\r\no={value}\r\n").as_bytes())
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
/// offers so far and its `o=` value) repeats the one the stack last restated
/// on the leg — whether that one left in a response or in a request (the
/// author's nested offer repeated by its final).
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
