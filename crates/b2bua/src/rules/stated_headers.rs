//! A call's stated headers ([`call::features::StatedHeaders`]) on the messages
//! the stack sends. Each message takes the scopes that name it — every message
//! of a leg that is not a media leg, then the originator's finals to its initial
//! INVITE or the initial INVITE of an originated leg — the wider scope applied
//! first and the narrower one resolved against its result. A header the stack,
//! the transaction or a negotiation binds is never restated, nor a capability
//! advertisement past its mint ([`sip_message::header::Restatement`]).
//! A leg with a set of its own takes it in place of the call's. `100 Trying`
//! and every message the transaction layer builds on its own (a CANCEL's
//! `200`, the `487` answering the cancelled INVITE, the release-time answer to
//! a pending INVITE, the ACK of a non-2xx final) take none.
//!
//! The stamp runs once, at the end of the turn, and a retained emission is the
//! message as it left: the stamp rebinds a retained copy of a message it changes.

use call::features::StatedHeaders;
use call::header_update::{HeaderUpdate, SipHeaderUpdates};
use call::{Call, LegKind};
use sip_message::draft::StartKind;
use sip_message::header::{HeaderName, Restatement};
use sip_message::{Method, SipRequest, SipResponse};

use crate::effects::{HandlerResult, OutboundBody, OutboundSipEffect};

/// Stamp every outbound message of `result` with its call's stated headers,
/// rebinding any retained copy of a message the stamp changes.
pub fn stamp_outbound(mut result: HandlerResult) -> HandlerResult {
    for effect in &mut result.effects.outbound {
        let before = image(effect);
        stamp(&result.call, effect);
        if let (Some(was), Some(now)) = (before, image(effect)) {
            if was != now {
                call::helpers::restate_retained(&mut result.call, &was, &now);
            }
        }
    }
    result
}

/// The call's termination begins: a tentative set gives way to the one it
/// replaced, the per-leg sets staying.
pub fn withdraw_tentative(call: &mut Call) {
    let Some(stated) = call.features.as_mut().and_then(|f| f.stated_headers.as_mut()) else {
        return;
    };
    let Some(mut earlier) = stated.reverts_to.take().map(|b| *b) else {
        return;
    };
    earlier.legs = std::mem::take(&mut stated.legs);
    *stated = earlier;
}

/// The bytes `effect` leaves as, for a request or a response.
fn image(effect: &OutboundSipEffect) -> Option<Vec<u8>> {
    match &effect.body {
        OutboundBody::Request(r) => Some(r.image().to_vec()),
        OutboundBody::Response(r) => Some(r.image().to_vec()),
        OutboundBody::Datagram(_) => None,
    }
}

/// Stamp `effect` with what `call`'s stated headers state for it. A retained
/// datagram is left as it is: its bytes were stamped before they were retained.
pub fn stamp(call: &Call, effect: &mut OutboundSipEffect) {
    let Some(leg_id) = effect.leg_id.as_deref() else {
        return;
    };
    match &mut effect.body {
        OutboundBody::Request(req) => {
            if let Some(stamped) = stamped_request(call, leg_id, req) {
                *req = stamped;
            }
        }
        OutboundBody::Response(resp) => stamp_response(call, leg_id, resp),
        OutboundBody::Datagram(_) => {}
    }
}

/// Stamp `resp`, sent on `leg_id`, with what `call`'s stated headers state
/// for it.
pub fn stamp_response(call: &Call, leg_id: &str, resp: &mut SipResponse) {
    if let Some(stamped) = stamped_response(call, leg_id, resp) {
        *resp = stamped;
    }
}

/// Stamp `answer`, the stack's own answer to `req`, with what `call`'s stated
/// headers state for it on the leg `req` arrived on (the leg holding its
/// Call-ID).
pub fn stamp_answer(call: &Call, req: &SipRequest, answer: &mut SipResponse) {
    let call_id = req.call_id().as_str();
    let leg = std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .find(|l| l.call_id == call_id)
        .map(|l| l.leg_id.clone());
    if let Some(leg_id) = leg {
        stamp_response(call, &leg_id, answer);
    }
}

/// The stated headers that reach `leg_id` — its own set, else the call's:
/// `None` for a media leg, a leg the call does not hold, or a call stating
/// none.
fn reaching<'a>(call: &'a Call, leg_id: &str) -> Option<(&'a StatedHeaders, LegKind)> {
    let stated = call.features.as_ref()?.stated_headers.as_ref()?;
    let leg = call::helpers::find_leg(call, leg_id)?;
    let kind = call::helpers::leg_kind(leg);
    (kind != LegKind::Media).then_some((stated.of_leg(leg_id), kind))
}

fn carried<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines.map(str::to_string).collect()
}

fn stamped_request(call: &Call, leg_id: &str, req: &SipRequest) -> Option<SipRequest> {
    let (stated, kind) = reaching(call, leg_id)?;
    let launched = kind != LegKind::A && req.method() == Method::Invite && req.to().tag().is_none();
    let mut sets = vec![&stated.every_message];
    if launched {
        sets.push(&stated.launched_invite);
    }
    restated(&sets, |name| carried(req.raw(name)), || req.thaw())?.freeze().ok()
}

fn stamped_response(call: &Call, leg_id: &str, resp: &SipResponse) -> Option<SipResponse> {
    if resp.status() == 100 {
        return None;
    }
    let (stated, kind) = reaching(call, leg_id)?;
    let final_to_initial = kind == LegKind::A
        && resp.status() >= 200
        && resp.cseq().method() == Method::Invite
        && resp.cseq().seq() == call.a_leg_invite.cseq;
    let mut sets = vec![&stated.every_message];
    if final_to_initial {
        sets.push(&stated.originator_finals);
    }
    restated(&sets, |name| carried(resp.raw(name)), || resp.thaw())?.freeze().ok()
}

/// `lines` once `update` is applied: a set's lines, none for a removal, an
/// add's lines where `lines` holds none.
fn applied(update: &HeaderUpdate, lines: Vec<String>) -> Vec<String> {
    match update {
        HeaderUpdate::Add(add) if lines.is_empty() => add.clone(),
        HeaderUpdate::Add(_) => lines,
        stated => stated.lines().to_vec(),
    }
}

/// The message's draft with `sets` applied in order — each set's statement of
/// a name resolved against what the sets before it left — or `None` where the
/// message already carries the result. Names compare case-insensitively;
/// `carried` reads the message's lines of a name, `thaw` opens its draft.
fn restated<D: Draft>(
    sets: &[&SipHeaderUpdates],
    carried: impl Fn(HeaderName) -> Vec<String>,
    thaw: impl FnOnce() -> D,
) -> Option<D> {
    let mut names: Vec<&str> = Vec::new();
    for name in sets.iter().flat_map(|set| set.keys()) {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name);
        }
    }
    let mut edits: Vec<(HeaderName, Vec<String>)> = Vec::new();
    for name in names {
        if HeaderName::restatement_of(name) != Restatement::Free {
            continue;
        }
        let header = HeaderName::from(name);
        let start = carried(header.clone());
        let mut lines = start.clone();
        for set in sets {
            if let Some((_, update)) = set.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                lines = applied(update, lines);
            }
        }
        if lines != start {
            edits.push((header, lines));
        }
    }
    if edits.is_empty() {
        return None;
    }
    let draft = edits.into_iter().fold(thaw(), |draft, (name, lines)| {
        let draft = draft.remove_all(&name);
        lines.into_iter().fold(draft, |d, line| d.push_line(name.clone(), line))
    });
    Some(draft)
}

/// The two draft operations a restatement needs, over a request or a response
/// draft.
trait Draft: Sized {
    fn remove_all(self, name: &HeaderName) -> Self;
    fn push_line(self, name: HeaderName, line: String) -> Self;
}

impl<S: StartKind> Draft for sip_message::draft::Draft<S> {
    fn remove_all(self, name: &HeaderName) -> Self {
        self.remove(name)
    }
    fn push_line(self, name: HeaderName, line: String) -> Self {
        self.push_raw(name, line)
    }
}

/// The INVITE `effect` mints on `leg`, with every add of `adds` whose name it
/// carries no line of; the leg's handles hold the INVITE as it leaves.
pub fn add_to_minted(
    leg: &mut call::Leg,
    effect: &mut OutboundSipEffect,
    adds: &[(String, Vec<String>)],
) {
    if adds.is_empty() {
        return;
    }
    if let OutboundBody::Request(req) = &mut effect.body {
        let was = req.image().to_vec();
        *req = with_adds(req.clone(), adds);
        call::helpers::restate_leg_invite(leg, &was, req.image());
    }
}

/// `req` with every add of `adds` whose name it carries no line of: the
/// lines a builder states only where the built message lacks the name.
pub fn with_adds(req: SipRequest, adds: &[(String, Vec<String>)]) -> SipRequest {
    let set = as_adds(adds);
    restated(&[&set], |name| carried(req.raw(name)), || req.thaw())
        .and_then(|d| d.freeze().ok())
        .unwrap_or(req)
}

/// `resp` with every add of `adds` whose name it carries no line of.
pub fn response_with_adds(resp: SipResponse, adds: &[(String, Vec<String>)]) -> SipResponse {
    let set = as_adds(adds);
    restated(&[&set], |name| carried(resp.raw(name)), || resp.thaw())
        .and_then(|d| d.freeze().ok())
        .unwrap_or(resp)
}

fn as_adds(adds: &[(String, Vec<String>)]) -> SipHeaderUpdates {
    adds.iter().map(|(n, l)| (n.clone(), HeaderUpdate::Add(l.clone()))).collect()
}

#[cfg(test)]
mod tests;
