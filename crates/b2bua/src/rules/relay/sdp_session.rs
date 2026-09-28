//! The session description seam: every description this stack puts on a leg
//! passes here on its way out, and every description a leg's peer sends is
//! noted here on its way in, so each dialog carries ONE session whoever
//! authored what crosses it (RFC 3264 §8).
//!
//! A description carrying the sess-id the stack last stated on the dialog
//! leaves as written — its author continues that session, so a plain relay
//! stays byte-transparent. Once the dialog is confirmed, one naming another
//! session (a transfer target, a rerouted destination, a media server, a body
//! of the stack's own) is restated under the stated session
//! ([`sip_message::restate_session`]). What the peer then describes on that
//! dialog travels on in the order of the restated author's streams, without
//! the slots that author never described ([`sip_message::in_author_order`]).
//! Before confirmation (the initial INVITE, its provisionals and final) a
//! description is only recorded.

use std::borrow::Cow;

use call::{Call, Leg, LegSdpSession, LegState};
use sip_message::header::MediaType;
use sip_message::multipart::sdp_range;
use sip_message::{in_author_order, parse_origin, restate_session, Method, StatedSession};

/// `body`, typed `content_type`, as it leaves on `leg_id` in a `method`
/// request or a response to one, with the leg's session state updated to what
/// it says. `within_dialog` is false for the initial INVITE and its responses,
/// which open the session rather than continue it. A body that carries no
/// session description, or one outside an offer/answer exchange (an OPTIONS
/// answer describes capabilities, RFC 3261 §11.2), leaves as it is.
pub fn continue_on_leg(
    call: &mut Call,
    leg_id: &str,
    method: &Method,
    within_dialog: bool,
    body: Vec<u8>,
    content_type: Option<&MediaType>,
) -> Vec<u8> {
    if !negotiates(method) {
        return body;
    }
    let Some(range) = content_type.and_then(|ct| sdp_range(ct, &body)) else {
        return body;
    };
    let sdp = &body[range.clone()];
    let reordered = within_dialog.then(|| in_restated_author_order(call, leg_id, sdp)).flatten();
    let sdp = reordered.as_deref().unwrap_or(sdp);
    let Some(leg) = leg_mut(call, leg_id) else {
        return body;
    };
    let restated = (within_dialog && leg.state == LegState::Confirmed)
        .then(|| stated(leg))
        .flatten()
        .and_then(|stated| restate_session(sdp, &stated));
    let (out, slots) = match restated {
        Some(r) => (Cow::Owned(r.sdp), r.slots),
        None => (Cow::Borrowed(sdp), Vec::new()),
    };
    if let Some(now) = StatedSession::of(&out) {
        leg.sdp_session.sent_origin = Some(now.origin);
        leg.sdp_session.sent_media = now.media;
        leg.sdp_session.sent_slots = slots;
    }
    if matches!(out, Cow::Borrowed(_)) && reordered.is_none() {
        return body;
    }
    [&body[..range.start], &out, &body[range.end..]].concat()
}

/// The session state of a leg this stack opens with `body` (typed
/// `content_type`): what its initial INVITE states, nothing yet received.
pub fn opened(body: &[u8], content_type: Option<&MediaType>) -> LegSdpSession {
    let stated =
        content_type.and_then(|ct| sdp_range(ct, body)).and_then(|r| StatedSession::of(&body[r]));
    LegSdpSession {
        sent_origin: stated.as_ref().map(|s| s.origin.clone()),
        sent_media: stated.map(|s| s.media).unwrap_or_default(),
        ..LegSdpSession::default()
    }
}

/// Note the session description `leg_id`'s peer sent in `body` (typed
/// `content_type`) in a `method` request or a response to one: its sess-id
/// names the descriptions that peer authors when they travel on. One outside
/// an offer/answer exchange names nothing.
pub fn note_received(
    call: &mut Call,
    leg_id: &str,
    method: &Method,
    body: &[u8],
    content_type: Option<&MediaType>,
) {
    if !negotiates(method) {
        return;
    }
    let Some(session_id) = content_type
        .and_then(|ct| sdp_range(ct, body))
        .and_then(|r| parse_origin(&body[r]))
        .map(|o| o.session_id)
    else {
        return;
    };
    if let Some(leg) = leg_mut(call, leg_id) {
        leg.sdp_session.received_session_id = Some(session_id);
    }
}

/// `sdp` in the stream order of the author the stack last restated toward
/// the leg `sdp` comes from, or `None` where that leg's last description was
/// its author's own. The leg is the one whose peer last stated `sdp`'s sess-id.
fn in_restated_author_order(call: &Call, leg_id: &str, sdp: &[u8]) -> Option<Vec<u8>> {
    let session_id = parse_origin(sdp)?.session_id;
    let from = std::iter::once(&call.a_leg).chain(&call.b_legs).find(|l| {
        l.leg_id != leg_id && l.sdp_session.received_session_id.as_deref() == Some(&session_id)
    })?;
    (!from.sdp_session.sent_slots.is_empty())
        .then(|| in_author_order(sdp, &from.sdp_session.sent_slots))
        .flatten()
}

/// The methods whose requests and responses carry offers and answers: INVITE
/// and its ACK (RFC 3264 §4), PRACK (RFC 3262 §5) and UPDATE (RFC 3311 §5).
fn negotiates(method: &Method) -> bool {
    matches!(method, Method::Invite | Method::Ack | Method::Prack | Method::Update)
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
