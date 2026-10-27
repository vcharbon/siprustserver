//! A driver step — what reaches the call next — and the event it makes of
//! the call as it stands and of what its peers have heard.

use b2bua_sdk::event::CallEvent;
use call::{Call, TimerType};
use sip_message::{Method, SipMessage};

use super::peers::{
    address_of, callee_tag, contact_of, line, method_body, method_lines, parsed, reliable, request,
    request_on, response, Peers, ALICE, ALICE_TAG, BOB, CALL_ID,
};
use crate::answer_deadline::CONSULT_CHANGE;

/// One step of a script, from a peer or the call's own clock. A leg names
/// its peer: `a` is alice, `b-1` bob, any other b-leg the callee it dialled.
#[derive(Clone, Debug)]
pub enum Step {
    /// The callee on the leg answers the INVITE that dialled it, under its
    /// own To-tag.
    Answers(&'static str, u16),
    /// [`Step::Answers`] with these lines added, `(name, value)`: a
    /// Record-Route, say (RFC 3261 §12.1.1).
    AnswersWith(&'static str, u16, Vec<(&'static str, &'static str)>),
    /// The callee on the leg answers that INVITE with a reliable provisional
    /// carrying the answer (RFC 3262).
    AnswersReliably(&'static str, u16),
    /// A fork of bob's INVITE answers it under the fork's own To-tag (RFC
    /// 3261 §12.1.2).
    ForkAnswers(&'static str, u16),
    /// That fork's reliable provisional (RFC 3262).
    ForkAnswersReliably(&'static str, u16),
    /// The peer on the leg answers the last request other than an ACK the
    /// stack sent it.
    AnswersSent(&'static str, u16),
    /// The peer on the leg answers the last request of this method the stack
    /// sent it.
    AnswersLast(&'static str, Method, u16),
    /// The peer on the leg answers the request of this method the stack sent
    /// it, counted from its first (0).
    AnswersHeard(&'static str, Method, usize, u16),
    /// The peer on the leg sends a request of this method on its dialog: an
    /// INVITE or an UPDATE carries an offer, a REFER a target (RFC 3515).
    Sends(&'static str, Method),
    /// [`Step::Sends`] with no body: an INVITE or an UPDATE that offers
    /// nothing.
    SendsBare(&'static str, Method),
    /// alice's ACK of the 2xx to her last INVITE.
    AliceAcks,
    /// alice's ACK of a non-2xx final to her first INVITE, on its branch
    /// (RFC 3261 §17.1.1.3).
    AliceAcksRefusal,
    /// alice's CANCEL of her INVITE, which the transaction layer answered.
    AliceCancels,
    /// A fire of the call's timer of this type, at the generation it holds.
    Fires(TimerType),
    /// A fire of the call's timer of this type on this leg.
    FiresOn(TimerType, &'static str),
    /// The call's clock moves on by this many milliseconds; no turn.
    Advance(i64),
    /// The `/call/failure` consult's answer, `outcome` with `payload`.
    FailureAnswers { outcome: &'static str, payload: serde_json::Value },
    /// The `/call/refer` consult's answer, `outcome` with `payload`.
    ReferAnswers { outcome: &'static str, payload: serde_json::Value },
    /// The reaper's verdict on the call, of this outcome
    /// ([`crate::reaper_verdict`]).
    Reaper(&'static str),
    /// An internal event of `topic` on the leg, `outcome` with `payload`.
    Internal {
        leg: &'static str,
        topic: &'static str,
        outcome: &'static str,
        payload: serde_json::Value,
    },
    /// The transaction layer's timeout of the last request of this method the
    /// stack sent the leg's peer, which drew no final.
    TimesOut(&'static str, Method),
    /// SIP text the leg's peer sends as written.
    Raw(&'static str, String),
}

/// What a step makes, as it reaches the call: the event and the leg it
/// arrived on.
pub(super) type Arrival = (CallEvent, String);

/// The step as it reaches the call standing as `call`, or `None` when the
/// call gives the step nothing to say (no dialog to say it on, nothing sent
/// to answer, no timer of the type armed). A step that sends a request
/// numbers it on its peer.
pub(super) fn event_of(step: &Step, call: &Call, peers: &mut Peers) -> Option<Arrival> {
    match step {
        Step::Answers(leg, status) => callee_final(peers, leg, *status, &callee_tag(leg), false),
        Step::AnswersWith(leg, status, lines) => {
            let extra = lines.iter().map(|(name, value)| line(name, value)).collect();
            let invite = peers.on(leg)?.dialled_by.as_ref()?;
            sip(response(invite, leg, *status, &callee_tag(leg), extra)?, leg)
        }
        Step::AnswersReliably(leg, status) => {
            callee_final(peers, leg, *status, &callee_tag(leg), true)
        }
        Step::ForkAnswers(tag, status) => callee_final(peers, "b-1", *status, tag, false),
        Step::ForkAnswersReliably(tag, status) => callee_final(peers, "b-1", *status, tag, true),
        Step::AnswersSent(leg, status) => {
            let req = peers.on(leg)?.last_heard()?;
            sip(response(req, leg, *status, &callee_tag(leg), Vec::new())?, leg)
        }
        Step::AnswersLast(leg, method, status) => {
            let req = peers.on(leg)?.last_of(method)?;
            sip(response(req, leg, *status, &callee_tag(leg), Vec::new())?, leg)
        }
        Step::AnswersHeard(leg, method, nth, status) => {
            let req = peers.on(leg)?.heard.iter().filter(|r| r.method() == method).nth(*nth)?;
            sip(response(req, leg, *status, &callee_tag(leg), Vec::new())?, leg)
        }
        Step::Sends(leg, method) => sends(call, peers, leg, method, method_body(method)),
        Step::SendsBare(leg, method) => sends(call, peers, leg, method, ""),
        Step::AliceAcks => {
            let own = call.a_leg.own_tag.as_deref()?;
            let cseq = peers.on("a")?.invite_cseq;
            sip(request("ACK", cseq, ALICE, &alice_lines(own), ""), "a")
        }
        Step::AliceAcksRefusal => {
            let own = call.a_leg.own_tag.as_deref()?;
            let ack = request_on("ACK", 1, ALICE, "z9hG4bK-lane", &alice_lines(own), "");
            sip(ack, "a")
        }
        Step::AliceCancels => {
            let event = CallEvent::Cancelled {
                call_id: CALL_ID.into(),
                from_tag: ALICE_TAG.into(),
                invite_cseq: Some(1),
                in_dialog: false,
                headers: Vec::new(),
                to_tag: call.a_leg.own_tag.clone(),
            };
            Some((event, "a".to_string()))
        }
        Step::Fires(timer_type) => fire(call, |t| t.timer_type() == timer_type),
        Step::FiresOn(timer_type, leg) => {
            fire(call, |t| t.timer_type() == timer_type && t.key.leg_id() == Some(*leg))
        }
        Step::Advance(_) => None,
        Step::FailureAnswers { outcome, payload } => {
            let change = call.timers.iter().find_map(|t| match t.timer_type() {
                TimerType::FailureAnswer { change, .. } => Some(*change),
                _ => None,
            })?;
            let mut payload = payload.clone();
            if let Some(map) = payload.as_object_mut() {
                map.insert(CONSULT_CHANGE.into(), change.into());
            }
            Some((internal(call, "call-failure-result", outcome, payload), "a".to_string()))
        }
        Step::ReferAnswers { outcome, payload } => {
            let event = internal(call, "refer-http-result", outcome, payload.clone());
            Some((event, "b-1".to_string()))
        }
        Step::Reaper(outcome) => {
            let topic = crate::reaper_verdict::REAPER_TOPIC;
            Some((internal(call, topic, outcome, serde_json::json!({})), "a".to_string()))
        }
        Step::Internal { leg, topic, outcome, payload } => {
            Some((internal(call, topic, outcome, payload.clone()), leg.to_string()))
        }
        Step::TimesOut(leg, method) => {
            let req = peers.on(leg)?.last_of(method)?;
            let event = CallEvent::Timeout {
                branch: req.top_via().branch()?.to_string(),
                call_ref: Some(call.call_ref.clone()),
                leg_id: Some(leg.to_string()),
                method: Some(method.as_str().to_string()),
                cseq: req.cseq().seq(),
                destination: address_of(leg),
                timeout_kind: call::TxnTimeoutKind::Response,
                incarnation: None,
            };
            Some((event, leg.to_string()))
        }
        Step::Raw(leg, text) => sip(text.clone().into_bytes(), leg),
    }
}

/// The callee on `leg`'s `status` to the INVITE that dialled it, under the
/// To-tag `tag`, reliable when `reliably`.
fn callee_final(
    peers: &Peers,
    leg: &str,
    status: u16,
    tag: &str,
    reliably: bool,
) -> Option<Arrival> {
    let invite = peers.on(leg)?.dialled_by.as_ref()?;
    let extra = if reliably { reliable() } else { Vec::new() };
    sip(response(invite, leg, status, tag, extra)?, leg)
}

/// A request of `method` from the peer on `leg`, on its dialog.
fn sends(
    call: &Call,
    peers: &mut Peers,
    leg: &str,
    method: &Method,
    body: &str,
) -> Option<Arrival> {
    let (src, lines) = if leg == "a" {
        (ALICE, alice_lines(call.a_leg.own_tag.as_deref()?))
    } else {
        (BOB, callee_lines(peers, leg)?)
    };
    let contact = contact_of(leg);
    let peer = peers.on_mut(leg)?;
    peer.cseq += 1;
    if *method == Method::Invite {
        peer.invite_cseq = peer.cseq;
    }
    let typed = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let lines = format!("{lines}{}{typed}", method_lines(method, &contact));
    sip(request(method.as_str(), peer.cseq, src, &lines, body), leg)
}

/// The dialog lines of alice's requests, the stack's tag `own` toward her.
fn alice_lines(own: &str) -> String {
    format!(
        "From: <sip:alice@example.com>;tag={ALICE_TAG}\r\n\
         To: <sip:bob@example.com>;tag={own}\r\n\
         Call-ID: {CALL_ID}\r\n"
    )
}

/// The dialog lines of the requests of the callee on `leg`.
fn callee_lines(peers: &Peers, leg: &str) -> Option<String> {
    let invite = peers.on(leg)?.dialled_by.as_ref()?;
    Some(format!(
        "From: <sip:bob@{BOB}>;tag={}\r\n\
         To: <sip:alice@example.com>;tag={}\r\n\
         Call-ID: {}\r\n",
        callee_tag(leg),
        invite.from().tag()?,
        invite.call_id().as_str()
    ))
}

/// A fire of the first timer of `call`'s ledger `which` picks, on its leg.
fn fire(call: &Call, which: impl Fn(&call::TimerEntry) -> bool) -> Option<Arrival> {
    let armed = call.timers.iter().find(|t| which(t))?;
    let event = CallEvent::Timer {
        key: armed.key.clone(),
        generation: armed.generation,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    Some((event, armed.key.leg_id().unwrap_or("a").to_string()))
}

/// The SIP text `raw` arriving from `leg`'s peer.
fn sip(raw: Vec<u8>, leg: &str) -> Option<Arrival> {
    Some((CallEvent::sip(parsed(&raw)?, address_of(leg)?), leg.to_string()))
}

/// An internal event of `topic` for `call`.
fn internal(call: &Call, topic: &str, outcome: &str, payload: serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: topic.into(),
        outcome: outcome.into(),
        payload,
        body: Vec::new(),
        incarnation: None,
    }
}

/// The SIP text of `event` rewritten by `edit`, read back; `None` when the
/// parser refuses it.
pub(super) fn edited(event: CallEvent, edit: &dyn Fn(&[u8]) -> Vec<u8>) -> Option<CallEvent> {
    let CallEvent::Sip { message, src } = event else {
        return Some(event);
    };
    Some(CallEvent::sip(parsed(&edit(message.image()))?, src))
}

/// The SIP text `event` carries, if any.
pub(super) fn text_of(event: &CallEvent) -> Option<Vec<u8>> {
    match event {
        CallEvent::Sip { message, .. } => Some(message.image().to_vec()),
        _ => None,
    }
}

/// Whether `event` is a response.
pub(super) fn is_response(event: &CallEvent) -> bool {
    matches!(event, CallEvent::Sip { message, .. } if matches!(message.as_ref(), SipMessage::Response(_)))
}
