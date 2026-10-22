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
//! restated. The same author's same version again within one exchange the
//! peer opened (a repeated provisional, the final repeating it) is the same
//! description: restated at the version already given. Each INVITE of the
//! peer, offerless included, and each UPDATE offer opens a new exchange, in
//! which even an unchanged description is the next version ([`note_request`]
//! names the requests that open none). What the peer then describes travels
//! back to that author in the author's stream order, without the slots the
//! author never described ([`sip_message::in_author_order`]). Before
//! confirmation (the initial INVITE, its provisionals and final) the first
//! description in a dialog opens its session; a later one there by another
//! author, or under another sess-id, is restated as that session's next
//! version (one early dialog, one session).
//!
//! What a peer receives continues what it last received: where the stack
//! answered an offer of the author itself — refused it or answered it locally
//! ([`note_absorbed_offer`]) — the author's own next description, stating a
//! version past that offer's, is restated — a repeat of what the peer holds at
//! that version, a change at the next. A version the author skipped on its own
//! account leaves as written: the skip is the author's.
//!
//! Inside an early dialog, a description of the stack's own that already
//! states the next version of the session the dialog carries (an answer it
//! composed as that session's author would) leaves as written and counts as a
//! version the stack stated; the author's later descriptions in that dialog
//! are then restated above it as on a confirmed one. Each early dialog of a
//! leg keeps its own session state until one of them confirms (RFC 3261
//! §12.1.2): a leg the stack dialled tells them apart by the peer's tag, the
//! caller's leg by this stack's.
//!
//! A description inside a dialog leaves in the form the service's
//! [`SdpFormPolicy`] gives it from what the seam knows ([`SdpCrossing`]): as
//! written, or re-serialized ([`sip_message::canonical_form`]). An opening
//! description always leaves as written. Whether the stack has restated
//! anything in the call is the call's fact: each leg records the
//! restatements it carried, and a confirming dialog takes over the facts of
//! the forks it replaces.

use std::borrow::Cow;

use call::{Call, DialogId, Leg, LegSdpSession, LegState};

use super::sdp_dialogs::{early_dialog_mut, enter_early_dialog};
use sip_message::SdpOrigin;

use crate::config::{SdpCrossing, SdpForm, SdpFormPolicy};
use crate::effects::{OutboundBody, OutboundSipEffect};
use sip_message::header::MediaType;
use sip_message::multipart::sdp_range;
use sip_message::{
    canonical_form, in_author_order, parse_origin, restate_session, restate_session_again, Method,
    SipRequest, StatedSession,
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
    /// In the initial INVITE or its provisionals and final: the first in a
    /// dialog opens the session; a later one continues it.
    Opening,
    /// In a later request of the dialog, or the initial ACK.
    InDialog,
    /// In a response of the dialog to the leg's peer: the same author's same
    /// version as the last restatement, in no newer exchange that peer opened
    /// (a repeated provisional, the final repeating what a reliable
    /// provisional or a nested UPDATE/PRACK exchange already carried), is the
    /// same description. A description in a request, or in the response to a
    /// newer exchange the peer opened (its INVITE, offerless included, or its
    /// UPDATE offer), is a new version.
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
/// `leg_id` inside its dialog `dialog` (`None`: the leg's one dialog, or none
/// yet) where it stands `carried`, with that dialog's session state updated to
/// what it says. While `leg_id` is unconfirmed, each of its early dialogs
/// carries its own state: on a leg this stack dialled the opening INVITE's
/// until something crosses it, told apart by the peer's tag; on the caller's
/// leg none until a description crosses it, told apart by this stack's tag
/// (RFC 3261 §12.1.2). The leg keeps the opening state, and
/// [`adopt_confirmed_dialog`](super::adopt_confirmed_dialog) /
/// [`adopt_answered_caller_dialog`](super::adopt_answered_caller_dialog) hand it
/// the confirming dialog's. A body that carries no session description leaves as
/// it is.
/// A version at the top of its range has no next one: a description that would
/// be restated above it leaves as written, and opens the session anew where the
/// stack had stated no version of its own (RFC 3264 §8 — never a wrapped one).
/// `policy` decides the form of an in-dialog description.
#[allow(clippy::too_many_arguments)]
pub fn continue_on_leg(
    call: &mut Call,
    leg_id: &str,
    dialog: Option<&DialogId>,
    author: Author<'_>,
    carried: Carried,
    body: Vec<u8>,
    content_type: Option<&MediaType>,
    policy: &dyn SdpFormPolicy,
) -> Vec<u8> {
    let call_restated = has_restated(call);
    let formed = Formed { policy, call_restated };
    let caller = call.a_leg.leg_id == leg_id;
    let Some(opening) = dialog.and_then(|id| enter_early_dialog(call, leg_id, caller, id)) else {
        return continue_session(call, leg_id, author, carried, body, content_type, formed);
    };
    let out = continue_session(call, leg_id, author, carried, body, content_type, formed);
    if let (Some(id), Some(leg)) = (dialog, leg_mut(call, leg_id)) {
        let exchanges_opened = leg.sdp_session.exchanges_opened;
        let own =
            std::mem::replace(&mut leg.sdp_session, LegSdpSession { exchanges_opened, ..opening });
        if let Some(d) = early_dialog_mut(leg, caller, id) {
            d.sdp_session = Some(own);
        }
    }
    out
}

/// [`continue_on_leg`] on the session state in the leg's place.
fn continue_session(
    call: &mut Call,
    leg_id: &str,
    author: Author<'_>,
    carried: Carried,
    body: Vec<u8>,
    content_type: Option<&MediaType>,
    formed: Formed<'_>,
) -> Vec<u8> {
    if carried == Carried::Outside {
        return body;
    }
    let Some(range) = content_type.and_then(|ct| sdp_range(ct, &body)) else {
        return body;
    };
    let Some(sdp) = body.get(range.clone()) else {
        return body;
    };
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
    let absorbed = match author {
        Author::Leg(from) => {
            leg_ref(call, from).and_then(|l| l.sdp_session.absorbed_origin.clone())
        }
        Author::Stack => None,
    };
    let Some(leg) = leg_mut(call, leg_id) else {
        return body;
    };
    let repeat_key = carried
        .in_dialog()
        .then(|| parse_origin(sdp))
        .flatten()
        .map(|o| format!("{} {}", leg.sdp_session.exchanges_opened, o.value()));
    let already_stated = author == Author::Stack
        && carried.in_dialog()
        && matches!(leg.state(), LegState::Trying | LegState::Early)
        && stated(leg).is_some_and(|s| states_next_version(sdp, &s.origin));
    let own = continues_itself(&leg.sdp_session, author, &session_id);
    let skips = own && skips_absorbed(&leg.sdp_session, sdp, absorbed.as_deref());
    let in_session = match carried {
        Carried::Opening => leg.sdp_session.sent_origin.is_some(),
        _ if carried.in_dialog() => match leg.state() {
            LegState::Confirmed => true,
            LegState::Trying | LegState::Early => leg.sdp_session.restated || skips,
            LegState::Terminated => false,
        },
        _ => false,
    };
    let restated = (in_session && !already_stated && (!own || skips))
        .then(|| stated(leg))
        .flatten()
        .and_then(|stated| {
            let repeat = if skips {
                repeats_what_was_held(&leg.sdp_session, sdp)
            } else {
                carried == Carried::Answering
                    && repeats(&leg.sdp_session, author, repeat_key.as_deref())
            };
            if repeat {
                restate_session_again(sdp, &stated)
            } else {
                restate_session(sdp, &stated)
            }
        });
    let reformed = carried.in_dialog()
        && formed.policy.form(&SdpCrossing {
            by_peer: matches!(author, Author::Leg(_)),
            restated: restated.is_some(),
            call_restated: formed.call_restated || restated.is_some(),
        }) == SdpForm::Canonical;
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
            state.has_restated = true;
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
        state.sent_digest = parse_origin(&out).map(|o| digest(&o.body_excluding_origin));
    }
    let out = match reformed.then(|| canonical_form(&out)).flatten() {
        Some(canonical) => Cow::Owned(canonical),
        None => out,
    };
    if matches!(out, Cow::Borrowed(_)) && reordered.is_none() {
        return body;
    }
    let mut joined = Vec::with_capacity(body.len().saturating_sub(range.len()) + out.len());
    joined.extend(body.iter().take(range.start));
    joined.extend_from_slice(&out);
    joined.extend(body.iter().skip(range.end));
    joined
}

/// Count the offer/answer exchange `req`, received from `leg_id`'s peer,
/// opens: an INVITE, offerless included (RFC 3264 §5), unless it meets
/// [`call::helpers::invite_glare`]; an UPDATE carrying a description unless
/// it meets an UPDATE of that peer awaiting its final or an open INVITE offer
/// ([`b2bua_sdk::open_offer::open_offer`], RFC 3311 §5.2). Nothing else.
/// `source` is the dialog the request names, as the dialog gate resolved it.
pub fn note_request(call: &mut Call, leg_id: &str, req: &SipRequest, source: Option<&DialogId>) {
    let opens = match req.method() {
        Method::Invite => !call::helpers::invite_glare(call, leg_id, source),
        Method::Update => {
            req.sdp().is_some()
                && !call::helpers::peer_update_pending(call, leg_id, source)
                && b2bua_sdk::open_offer::open_offer(call, leg_id, source).is_none()
        }
        _ => false,
    };
    if let (true, Some(leg)) = (opens, leg_mut(call, leg_id)) {
        leg.sdp_session.exchanges_opened = leg.sdp_session.exchanges_opened.saturating_add(1);
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
pub(super) fn origin_of(value: &str) -> Option<SdpOrigin> {
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

/// Whether `sdp`, its author's own continuation of the session `state`
/// carries, states a version past `absorbed` — the `o=` value of the
/// author's last offer the stack answered itself — where that offer's version
/// lies above the one the peer holds: the peer never received it, so what it
/// receives is restated to continue what it holds (RFC 3264 §8).
fn skips_absorbed(state: &LegSdpSession, sdp: &[u8], absorbed: Option<&str>) -> bool {
    let (Some(now), Some(held), Some(absorbed)) = (
        parse_origin(sdp),
        state.sent_origin.as_deref().and_then(origin_of),
        absorbed.and_then(origin_of),
    ) else {
        return false;
    };
    now.identifies_same_session(&held)
        && absorbed.identifies_same_session(&held)
        && held.session_version < absorbed.session_version
        && absorbed.session_version < now.session_version
}

/// Whether `sdp` describes, apart from its `o=` line, what the peer last
/// received under `state`: a repeat, kept at the version the peer holds.
fn repeats_what_was_held(state: &LegSdpSession, sdp: &[u8]) -> bool {
    let now = parse_origin(sdp).map(|o| digest(&o.body_excluding_origin));
    now.is_some() && now == state.sent_digest
}

/// The 64-bit FNV-1a digest of `text`: stable across processes and builds,
/// so a replicated call compares alike on every node.
fn digest(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// Record that the stack answers `req`, received from `leg_id`'s peer,
/// itself: an offer it carries in an INVITE or UPDATE reaches no other leg
/// ([`note_absorbed_offer`]).
pub fn note_answered_here(call: &mut Call, leg_id: &str, req: &SipRequest) {
    if let Some(offer) =
        req.sdp().filter(|_| matches!(req.method(), Method::Invite | Method::Update))
    {
        note_absorbed_offer(call, leg_id, offer);
    }
}

/// Record that the stack answered `offer`, received from `leg_id`'s peer,
/// itself — refused it or answered it locally — so no other leg received its
/// version ([`call::LegSdpSession::absorbed_origin`]).
pub fn note_absorbed_offer(call: &mut Call, leg_id: &str, offer: &[u8]) {
    if let (Some(origin), Some(leg)) = (parse_origin(offer), leg_mut(call, leg_id)) {
        leg.sdp_session.absorbed_origin = Some(origin.value().to_string());
    }
}

/// `sdp`, written by the peer of `from` and going to `to`, in the stream order
/// of `to`'s description the stack last restated toward `from`, or `None`
/// where that description was not `to`'s (or not restated at all).
fn in_restated_author_order(call: &Call, from: &str, to: &str, sdp: &[u8]) -> Option<Vec<u8>> {
    let state = &leg_ref(call, from)?.sdp_session;
    (state.sent_slots_author.as_deref() == Some(to) && !state.sent_slots.is_empty())
        .then(|| in_author_order(sdp, &state.sent_slots))
        .flatten()
}

/// Whether a description by `author` under `key` (the count of the exchanges
/// the peer opened so far and its `o=` value) repeats the one the stack last
/// restated on the leg — whether that one left in a response or in a request
/// (the author's nested offer repeated by its final).
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

/// The policy deciding an in-dialog description's form, and whether the call
/// had a restatement before this description (read before any early-dialog
/// state takes the leg's place).
#[derive(Clone, Copy)]
struct Formed<'a> {
    policy: &'a dyn SdpFormPolicy,
    call_restated: bool,
}

/// Whether this stack has restated a description anywhere in `call`: on a leg,
/// or inside an early dialog of one.
fn has_restated(call: &Call) -> bool {
    std::iter::once(&call.a_leg).chain(&call.b_legs).any(|leg| {
        leg.sdp_session.has_restated
            || leg.dialogs.iter().any(|d| d.sdp_session.as_ref().is_some_and(|s| s.has_restated))
    })
}

fn leg_ref<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    std::iter::once(&call.a_leg).chain(&call.b_legs).find(|l| l.leg_id == leg_id)
}

pub(super) fn leg_mut<'a>(call: &'a mut Call, leg_id: &str) -> Option<&'a mut Leg> {
    std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()).find(|l| l.leg_id == leg_id)
}

#[cfg(test)]
#[path = "sdp_session_tests.rs"]
mod tests;
