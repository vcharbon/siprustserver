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
mod tests {
    use super::*;
    use crate::effects::{HandlerEffects, OutboundTxnMode, Provenance};
    use crate::router::test_support::{invite, src};

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|l| l.to_string()).collect()
    }

    fn map(entries: &[(&str, HeaderUpdate)]) -> SipHeaderUpdates {
        entries.iter().map(|(n, u)| (n.to_string(), u.clone())).collect()
    }

    /// What `sets` change on a message carrying `carried` (name → lines):
    /// `None` where nothing changes, else the names edited and their new lines.
    fn edits(
        sets: &[SipHeaderUpdates],
        carried: &[(&str, &[&str])],
    ) -> Option<Vec<(String, Vec<String>)>> {
        let sets: Vec<&SipHeaderUpdates> = sets.iter().collect();
        let carried_of = |name: HeaderName| -> Vec<String> {
            carried
                .iter()
                .filter(|(n, _)| HeaderName::from(*n).matches(name.as_wire_str()))
                .flat_map(|(_, l)| lines(l))
                .collect()
        };
        restated(&sets, carried_of, Vec::new)
    }

    /// A draft recording the edits it receives, by name.
    impl Draft for Vec<(String, Vec<String>)> {
        fn remove_all(mut self, name: &HeaderName) -> Self {
            self.push((name.as_wire_str().to_string(), vec![]));
            self
        }
        fn push_line(mut self, _name: HeaderName, line: String) -> Self {
            if let Some(last) = self.last_mut() {
                last.1.push(line);
            }
            self
        }
    }

    #[test]
    fn a_set_replaces_every_line_and_is_idempotent() {
        let set = map(&[("X-A", HeaderUpdate::Set(lines(&["1", "2"])))]);
        assert_eq!(
            edits(std::slice::from_ref(&set), &[("x-a", &["old"])]),
            Some(vec![("X-A".to_string(), lines(&["1", "2"]))])
        );
        assert_eq!(edits(std::slice::from_ref(&set), &[("X-A", &["1", "2"])]), None);
        assert_eq!(
            edits(&[set], &[("X-A", &["2", "1"])]),
            Some(vec![("X-A".to_string(), lines(&["1", "2"]))]),
            "line order is the statement"
        );
    }

    #[test]
    fn a_removal_takes_every_line_off_and_leaves_a_message_without_one() {
        for removal in [HeaderUpdate::Remove, HeaderUpdate::Set(vec![])] {
            let set = map(&[("X-A", removal)]);
            assert_eq!(
                edits(std::slice::from_ref(&set), &[("X-A", &["a", "b"])]),
                Some(vec![("X-A".to_string(), vec![])])
            );
            assert_eq!(edits(&[set], &[]), None);
        }
    }

    #[test]
    fn an_add_rides_only_a_message_carrying_none() {
        let set = map(&[("X-A", HeaderUpdate::Add(lines(&["add"])))]);
        assert_eq!(
            edits(std::slice::from_ref(&set), &[]),
            Some(vec![("X-A".to_string(), lines(&["add"]))])
        );
        assert_eq!(edits(&[set], &[("x-a", &["own"])]), None, "the carried line stands");
        assert_eq!(edits(&[map(&[("X-A", HeaderUpdate::Add(vec![]))])], &[]), None);
    }

    #[test]
    fn a_header_bound_to_the_stack_or_the_transaction_is_never_restated() {
        for name in ["Via", "Contact", "CSeq", "Require", "RSeq", "Content-Type", "Supported"] {
            assert_eq!(edits(&[map(&[(name, HeaderUpdate::line("x"))])], &[]), None, "{name}");
        }
    }

    #[test]
    fn the_wider_set_applies_first_and_the_narrower_against_its_result() {
        let every = map(&[
            ("X-A", HeaderUpdate::line("every")),
            ("X-B", HeaderUpdate::Remove),
            ("X-C", HeaderUpdate::Add(lines(&["every"]))),
        ]);
        let narrow = map(&[
            ("x-a", HeaderUpdate::line("narrow")),
            ("x-b", HeaderUpdate::Add(lines(&["narrow"]))),
            ("x-c", HeaderUpdate::Remove),
        ]);
        let got = edits(&[every, narrow], &[("X-B", &["carried"])]).unwrap();
        assert_eq!(
            got,
            vec![("X-A".to_string(), lines(&["narrow"])), ("X-B".to_string(), lines(&["narrow"])),],
            "X-C: added by the wider set, removed by the narrower — left as carried"
        );
    }

    /// A message retained before the turn's end and changed by the end-of-turn
    /// stamp — its set replaced later in the turn — is retained as it leaves.
    #[test]
    fn the_end_of_turn_stamp_rebinds_a_retained_copy_of_what_it_changes() {
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(
            &invite("w0", "w1", "stamp"),
            src(),
            &config,
            &ids,
            0,
        );
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let ack = a_invite.clone();
        let (emission, _) = call::RetainedEmission::paced(
            ack.image().to_vec(),
            ("192.0.2.1".into(), 5060),
            sip_retransmit::Class::Final2xx,
            call::Repeated::response("INVITE", 200),
        );
        call.reliable_provisionals.push(call::ReliableProvisional {
            a_tag: "t".into(),
            a_rseq: 1,
            b_leg_id: "b-1".into(),
            b_tag: "u".into(),
            b_cseq: 1,
            b_rseq: 1,
            acknowledged: false,
            emission: Some(emission),
            a_cseq: 1,
            carried_sdp: false,
            responder_sdp: false,
            responder_offer: None,
        });
        let mut stated = StatedHeaders::default();
        stated.every_message.insert("X-Late".into(), HeaderUpdate::line("late"));
        let mut features = crate::decision::default_platform_features();
        features.stated_headers = Some(stated);
        call.features = Some(features);
        let mut fx = HandlerEffects::new();
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(ack),
            mode: OutboundTxnMode::Raw,
            destination: ("192.0.2.1".into(), 5060),
            label: "retained".into(),
            leg_id: Some("a".into()),
            provenance: Provenance::Authored,
        });
        let result = stamp_outbound(HandlerResult { call, effects: fx });
        let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
        assert_eq!(sent.raw(HeaderName::from("X-Late")).collect::<Vec<_>>(), ["late"]);
        let retained = result.call.reliable_provisionals[0].emission.as_ref().unwrap();
        assert_eq!(retained.wire().0, sent.image().as_ref(), "the retained copy is what left");
    }

    /// The launched INVITE the end-of-turn stamp changes is the one the leg's
    /// client-transaction handle holds: a takeover seeds the client INVITE
    /// from that handle, and a retransmission of a request is the request it
    /// repeats (RFC 3261 §17.1.1.2).
    #[test]
    fn the_end_of_turn_stamp_rebinds_the_launched_invites_handle() {
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(
            &invite("w0", "w1", "handle"),
            src(),
            &config,
            &ids,
            0,
        );
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let (leg, effect) = crate::rules::relay::build_b_leg(
            crate::rules::relay::CallMarks::of(&call),
            "b-1",
            &a_invite,
            ("192.0.2.9".into(), 5060),
            None,
            None,
            None,
            None,
            &config,
            &ids,
            None,
            &[],
            &sip_message::generators::CapabilitySet::default(),
            None,
            &[],
            &[],
            None,
            true,
            0,
        )
        .expect("the leg is built");
        call.b_legs.push(leg);
        let mut stated = StatedHeaders::default();
        stated.every_message.insert("X-Stamp".into(), HeaderUpdate::line("s"));
        let mut features = crate::decision::default_platform_features();
        features.stated_headers = Some(stated);
        call.features = Some(features);
        let mut fx = HandlerEffects::new();
        fx.outbound.push(effect);
        let result = stamp_outbound(HandlerResult { call, effects: fx });
        let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
        assert_eq!(sent.raw(HeaderName::from("X-Stamp")).collect::<Vec<_>>(), ["s"]);
        let leg = &result.call.b_legs[0];
        let held = leg.pending_invite_txn.as_ref().expect("the leg's INVITE handle");
        assert_eq!(held.original_invite, sent.image().to_vec(), "the handle holds what left");
        for d in &leg.dialogs {
            if let Some(h) = &d.ext.pending_invite_txn {
                assert_eq!(h.original_invite, sent.image().to_vec());
            }
        }
    }

    /// A leg's own set stays with the call once the leg has ended: an ended
    /// leg may still owe messages (a 200 crossing its CANCEL, a BYE's answer).
    #[test]
    fn an_ended_legs_own_set_stays_with_the_call() {
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(
            &invite("w0", "w1", "prune"),
            src(),
            &config,
            &ids,
            0,
        );
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let (mut leg, effect) = crate::rules::relay::build_b_leg(
            crate::rules::relay::CallMarks::of(&call),
            "b-1",
            &a_invite,
            ("192.0.2.9".into(), 5060),
            None,
            None,
            None,
            None,
            &config,
            &ids,
            None,
            &[],
            &sip_message::generators::CapabilitySet::default(),
            None,
            &[],
            &[],
            None,
            true,
            0,
        )
        .expect("the leg is built");
        leg.state = call::LegState::Terminated;
        call.b_legs.push(leg);
        let mut stated = StatedHeaders::default();
        let mut own = StatedHeaders::default();
        own.every_message.insert("X-Own".into(), HeaderUpdate::line("leg"));
        stated.legs.insert("b-1".into(), own);
        stated.every_message.insert("X-Call".into(), HeaderUpdate::line("call"));
        let mut features = crate::decision::default_platform_features();
        features.stated_headers = Some(stated);
        call.features = Some(features);
        let mut fx = HandlerEffects::new();
        fx.outbound.push(effect);
        let result = stamp_outbound(HandlerResult { call, effects: fx });
        let OutboundBody::Request(sent) = &result.effects.outbound[0].body else { unreachable!() };
        assert_eq!(sent.raw(HeaderName::from("X-Own")).collect::<Vec<_>>(), ["leg"]);
        let left = result.call.features.unwrap().stated_headers.unwrap();
        assert_eq!(left.legs.len(), 1, "the ended leg's set stays: {left:?}");
        assert_eq!(left.every_message.len(), 1, "the call's set stays");
    }

    /// The charging arm reads the set of the leg it mints for: a leg whose own
    /// set states the vector mints none, whatever the call's set says, and one
    /// whose own set says nothing of it mints, whatever the call's set says.
    #[test]
    fn the_charging_arm_reads_the_legs_own_set() {
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let ids = sip_txn::IdGen::seeded(1);
        let mut call = crate::initial_invite::build_initial_call(
            &invite("w0", "w1", "arm"),
            src(),
            &config,
            &ids,
            0,
        );
        let mut stated = StatedHeaders::default();
        stated.every_message.insert("P-Charging-Vector".into(), HeaderUpdate::line("call"));
        let mut silent = StatedHeaders::default();
        silent.every_message.insert("X-Other".into(), HeaderUpdate::line("x"));
        stated.legs.insert("b-1".into(), silent);
        let mut stating = StatedHeaders::default();
        stating
            .every_message
            .insert("P-Charging-Vector".into(), HeaderUpdate::Add(vec!["t".into()]));
        stated.legs.insert("b-2".into(), stating);
        let mut features = crate::decision::default_platform_features();
        features.charging_vector = Some(call::features::ChargingVectorFeature::default());
        features.stated_headers = Some(stated.clone());
        call.features = Some(features);
        assert!(crate::rules::charging::minting_arm(&call, "b-1", None).is_some());
        assert!(crate::rules::charging::minting_arm(&call, "b-2", None).is_none());
        assert!(crate::rules::charging::minting_arm(&call, "b-3", None).is_none(), "the call's");
    }
}
