// ── AckLeg body + Content-Type (RFC 3261 §13.2.2.4 delayed-offer answer) ─────
//
// `RuleAction::AckLeg` carries an optional body so a rule can ACK a 2xx with an
// SDP answer, completing a delayed-offer exchange (RFC 3264 §4 — the answer
// rides the ACK). A body-bearing ACK defaults its Content-Type to
// `application/sdp`; an explicit type overrides it; an empty body stays a bare
// ACK (no body, no Content-Type). The body is a binary-safe `Vec<u8>`. Every
// emitted ACK retains its exact datagram on the dialog (`emitted_ack`), and a
// bodyless AckLeg re-passes those bytes raw (§13.2.2.4 — the re-ACK is THE ACK
// the 2xx triggered).
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use sip_message::HeaderName;

/// A confirmed b-leg (callee tag learned) whose first dialog `AckLeg` addresses.
fn b_leg_confirmed() -> Leg {
    let mut leg = b_leg_pending();
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    with_dialog(&mut leg, "bobtag");
    leg
}

/// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against a call
/// carrying a confirmed `b-1`, and return the emitted ACK. `AckLeg` reads only
/// the call + the action fields (not the event), so any event serves.
fn ack_request(body: Vec<u8>, content_type: Option<String>) -> SipRequest {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_confirmed());
    let config = B2buaConfig::default();
    let event = CallEvent::Timer {
        key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = context(&call, &event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let result = exec.execute(
        &[RuleAction::AckLeg {
            leg_id: "b-1".into(),
            body: Some(Body::new(body, content_type, BodyAuthor::Stack)),
        }],
        &call,
        &ctx,
    );
    let effect = result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "ACK"))
        .expect("AckLeg emits an ACK request");
    match &effect.body {
        OutboundBody::Request(r) => r.clone(),
        _ => unreachable!(),
    }
}

// A non-empty body rides the ACK verbatim (binary-safe) and, absent an
// explicit type, the ACK advertises `Content-Type: application/sdp`.
#[test]
fn ack_leg_carries_body_verbatim_and_defaults_content_type_to_sdp() {
    let mut body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\n\
          m=audio 40000 RTP/AVP 0\r\n"
        .to_vec();
    body.push(0xFF); // a non-UTF-8 byte must survive the Vec<u8> round-trip.

    let ack = ack_request(body.clone(), None);
    assert_eq!(
        &ack.body()[..],
        &body[..],
        "the delayed-offer answer rides the ACK byte-for-byte (binary-safe)"
    );
    assert_eq!(
        ack.raw(HeaderName::ContentType).next(),
        Some("application/sdp"),
        "a body-bearing ACK defaults Content-Type to application/sdp (§13.2.2.4)"
    );
}

// An explicit content_type overrides the application/sdp default.
#[test]
fn ack_leg_honours_explicit_content_type_override() {
    let body = b"<some>opaque</some>".to_vec();
    let ack = ack_request(body.clone(), Some("application/custom".to_string()));
    assert_eq!(&ack.body()[..], &body[..]);
    assert_eq!(
        ack.raw(HeaderName::ContentType).next(),
        Some("application/custom"),
        "an explicit content_type is used verbatim (not coerced to application/sdp)"
    );
}

// An empty body sends a bare ACK with no Content-Type.
#[test]
fn ack_leg_empty_body_sends_bare_ack_with_no_content_type() {
    let ack = ack_request(Vec::new(), None);
    assert!(ack.body().is_empty(), "an empty AckLeg body sends a bodyless ACK");
    assert_eq!(ack.raw(HeaderName::ContentType).next(), None, "a bare ACK carries no Content-Type");
}

// ── The retained ACK datagram (§13.2.2.4 re-ACK: THE ack, re-passed) ─────

/// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against `call`
/// and return the full [`HandlerResult`] (these tests read both the wire
/// effect and the updated call).
fn ack_exec(call: &call::Call, body: Vec<u8>, content_type: Option<String>) -> HandlerResult {
    let config = B2buaConfig::default();
    let event = CallEvent::Timer {
        key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = context(call, &event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    exec.execute(
        &[RuleAction::AckLeg {
            leg_id: "b-1".into(),
            body: Some(Body::new(body, content_type, BodyAuthor::Stack)),
        }],
        call,
        &ctx,
    )
}

/// The one emitted ACK effect in `result`.
fn ack_effect(result: &HandlerResult) -> &b2bua_core::effects::OutboundSipEffect {
    result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "ACK"))
        .expect("AckLeg emits an ACK request")
}

/// The one re-ACK in `result`: a retained datagram repeated as its bytes
/// (RFC 3261 §13.2.2.4), never a composed request.
fn re_ack_effect(result: &HandlerResult) -> &b2bua_core::effects::OutboundSipEffect {
    assert!(
        !result.effects.outbound.iter().any(|e| matches!(&e.body, OutboundBody::Request(_))),
        "a re-ACK composes nothing: {:?}",
        result.effects.outbound
    );
    result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Datagram(_)))
        .expect("the re-ACK repeats the retained datagram")
}

// A body-bearing ACK (the relayed delayed-offer answer) retains its exact
// wire bytes + destination on the dialog, so a §13.2.2.4 re-ACK can re-pass
// THE ACK instead of composing an answerless one.
#[test]
fn a_body_bearing_ack_retains_its_exact_datagram_and_destination() {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_confirmed());
    let result = ack_exec(&call, b"answer-sdp".to_vec(), None);
    let effect = ack_effect(&result);
    let OutboundBody::Request(sent) = &effect.body else { unreachable!() };
    let retained = result.call.b_legs[0]
        .dialogs
        .confirmed()
        .and_then(|d| call::helpers::sent_exchange(d)?.emitted_ack.clone())
        .expect("the emitted ACK datagram is retained on the dialog");
    let (bytes, dest) = retained.wire();
    assert_eq!(bytes, sent.image(), "retained bytes are the wire bytes");
    assert_eq!(
        dest,
        (effect.destination.0.as_str(), effect.destination.1),
        "retained destination is the wire destination"
    );
    assert_eq!(
        retained.repeat(),
        call::Repeat::OnTrigger,
        "an ACK is repeated only when a 2xx copy provokes it"
    );
}

// RFC 3261 §13.2.2.4: with a datagram retained, a bodyless AckLeg (the
// `re-ack-retransmitted-2xx` action) re-passes THE ACK raw — byte-identical,
// answer included, at the retained destination — never a fresh bodyless one.
#[test]
fn a_bodyless_ack_re_passes_the_retained_datagram_byte_identical() {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_confirmed());
    let first = ack_exec(&call, b"answer-sdp".to_vec(), None);
    let first_effect = ack_effect(&first);
    let OutboundBody::Request(sent) = &first_effect.body else { unreachable!() };
    let first_bytes = sent.image().to_vec();

    let second = ack_exec(&first.call, Vec::new(), None);
    let effect = re_ack_effect(&second);
    let OutboundBody::Datagram(re_sent) = &effect.body else { unreachable!() };
    assert_eq!(
        re_sent.wire().0,
        &first_bytes[..],
        "the re-ACK is the SAME datagram, answer included"
    );
    assert_eq!(
        (re_sent.repeat().ladder(), re_sent.repeated().method(), re_sent.repeated().code()),
        ("trigger", "ACK", None),
        "the re-ACK leaves labelled as a triggered repeat of an ACK",
    );
    let re_sent = re_sent.wire().0;
    assert_eq!(effect.destination, first_effect.destination, "same wire destination");
    assert!(
        re_sent.ends_with(b"answer-sdp"),
        "the delayed-offer answer rides the re-ACK (a fresh composition would drop it)"
    );
}

// The minted, bodyless first ACK retains its datagram too: the §13.2.2.4
// unit is the datagram regardless of body, so a copy of that 2xx draws the
// same bare bytes back.
#[test]
fn a_bare_first_ack_retains_its_datagram_and_a_re_ack_repeats_it() {
    let mut call = test_call();
    call::helpers::add_b_leg(&mut call, b_leg_confirmed());
    let first = ack_exec(&call, Vec::new(), None);
    let OutboundBody::Request(sent) = &ack_effect(&first).body else { unreachable!() };
    let retained = first.call.b_legs[0]
        .dialogs
        .confirmed()
        .and_then(|d| call::helpers::sent_exchange(d)?.emitted_ack.clone())
        .expect("the bare ACK is retained as well");
    assert_eq!(retained.wire().0, sent.image());
    assert!(sent.body().is_empty(), "a minted ACK stays bare");

    let second = ack_exec(&first.call, Vec::new(), None);
    let OutboundBody::Datagram(re_sent) = &re_ack_effect(&second).body else { unreachable!() };
    assert_eq!(re_sent.wire().0, sent.image(), "the re-ACK repeats the bare datagram");
}
