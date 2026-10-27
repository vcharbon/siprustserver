// ── Media/INFO primitives + leg-kind relay gate (ADR-0016) ───────────────────
//
// An unadopted `media` leg is gated out of the generic relay-to-peer `→ a`
// fallback, while an adopted leg still relays; `send-provisional-to-leg` brokers
// a 183 early-media onto the a-leg; and `send-request-to-leg` carries an opaque
// MSCML INFO body to a named leg.
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use b2bua_core::rules::MessageTransform;
use sip_message::HeaderName;
mod unbuilt_relay;

/// A confirmed b-leg of the given role; its single dialog carries the callee
/// tag so in-dialog originators have a confirmed dialog to ride.
fn confirmed_b_leg(leg_id: &str, kind: LegKind) -> Leg {
    let mut leg = super::fixtures::b_leg_pending();
    leg.leg_id = leg_id.into();
    call::testkit::stage_leg(&mut leg, LegState::Confirmed);
    leg.kind = Some(kind);
    leg.adopted = None; // derive adoption from the kind
    with_dialog(&mut leg, "bobtag").peer_cseq = Some(1);
    leg
}

/// The a-leg with a confirmed dialog, the target a relayed request toward A rides.
fn give_a_leg_dialog(call: &mut call::Call) {
    confirm_a(call);
    call.a_leg.dialogs = vec![a_dialog(call, "a-svc", DialogState::Confirmed)].into();
    call.a_leg.own_tag = Some("a-svc".into());
}

/// An in-dialog INFO request (carries a To-tag) with a DTMF payload.
fn in_dialog_info() -> SipRequest {
    let invite = super::fixtures::invite();
    invite
        .thaw()
        .with_method(Method::Info)
        .set(invite.to().clone().with_tag("svc"))
        .set(sip_message::header::CSeq::new(2, Method::Info))
        .body(
            sip_message::Bytes::from_static(b"Signal=5\r\nDuration=160\r\n"),
            sip_message::header::MediaType::new("application/dtmf-relay"),
        )
        .freeze()
        .expect("an in-dialog INFO of the same dialog is complete")
}

/// The statuses `result` answered with, in order.
fn answered(result: &HandlerResult) -> Vec<u16> {
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) => Some(r.status()),
            _ => None,
        })
        .collect()
}

// A caller request toward a peer dialog this stack's BYE is ending draws
// the final its method always takes where no relay could ever carry it
// (SUBSCRIBE: 489, RFC 6665 §4.2.1.2), and 500 + Retry-After where a
// relay could later (INFO, RFC 3261 §14.2).
#[test]
fn a_request_toward_an_ending_peer_takes_its_methods_final() {
    let mut call = test_call();
    give_a_leg_dialog(&mut call);
    let mut b = confirmed_b_leg("b-1", LegKind::Destination);
    for d in b.dialogs.iter_mut() {
        d.state = DialogState::Ending(call::EndingBy::Bye);
    }
    call = call::helpers::add_b_leg(call, b);
    call = call::helpers::merge_leg(call, "a", "b-1");
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    for (method, status) in
        [(sip_message::Method::Subscribe, 489), (sip_message::Method::Info, 500)]
    {
        let event = CallEvent::sip(
            SipMessage::Request(super::fixtures::in_dialog_request(method.clone())),
            "127.0.0.1:5060".parse().unwrap(),
        );
        let result = exec_on(
            &call,
            &event,
            "a",
            &config,
            &mut id_gen,
            &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
        );
        assert_eq!(answered(&result), vec![status], "{method:?}");
    }
}

// Relay-to-peer from an unadopted media leg relays nothing — it must NOT
// fall back to A — and its request is refused 403.
#[test]
fn relay_to_peer_is_gated_for_unadopted_media_leg() {
    let mut call = test_call();
    give_a_leg_dialog(&mut call);
    call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
    );
    let sent: Vec<Option<u16>> = result
        .effects
        .outbound
        .iter()
        .map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) => Some(r.status()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        vec![Some(403)],
        "an unadopted media leg's relay-to-peer is refused 403, never mis-routed to A"
    );
}

// An adopted (destination) leg still falls back to A — the gate only
// suppresses unadopted legs.
#[test]
fn relay_to_peer_falls_back_to_a_for_adopted_destination_leg() {
    let mut call = test_call();
    give_a_leg_dialog(&mut call);
    call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Destination));
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
    );
    assert_eq!(result.effects.outbound.len(), 1, "adopted leg relays to its peer");
    assert_eq!(
        result.effects.outbound[0].leg_id.as_deref(),
        Some("a"),
        "the relay falls back to the a-leg"
    );
}

// INFO with an opaque MSCML body is emitted verbatim to the named leg with
// the given content type (the MSCML control channel toward an MRF).
#[test]
fn send_request_to_leg_emits_info_with_mscml_body() {
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mscml = b"<MediaServerControl><request><play/></request></MediaServerControl>".to_vec();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::SendRequestToLeg {
            leg_id: "b-1".into(),
            method: "INFO".into(),
            body: Some(Body::new(
                mscml.clone(),
                Some("application/mediaservercontrol+xml".into()),
                BodyAuthor::Stack,
            )),
            headers: vec![],
        }],
    );
    assert_eq!(result.effects.outbound.len(), 1);
    let eff = &result.effects.outbound[0];
    assert_eq!(eff.leg_id.as_deref(), Some("b-1"));
    match &eff.body {
        OutboundBody::Request(r) => {
            assert_eq!(r.method(), "INFO");
            assert_eq!(&r.body()[..], &mscml[..], "MSCML body passes through opaquely");
            assert_eq!(
                r.raw(HeaderName::ContentType).next(),
                Some("application/mediaservercontrol+xml")
            );
        }
        _ => panic!("expected an outbound request"),
    }
}

// A service re-originating an in-dialog request forwards arbitrary application
// headers verbatim onto the request — the seam a deferred User-to-User relay uses
// to carry a held `User-To-User` toward the peer at an async decision re-entry.
// Body-owned headers (Content-Type/Content-Length) listed
// here are dropped, never duplicated: `body`/`content_type` own those.
#[test]
fn send_request_to_leg_forwards_arbitrary_headers() {
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let uui = "3030373946313233;encoding=hex"; // RFC 7433 User-To-User
    let body = b"SUP:example-binary\x00\x01payload".to_vec();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::SendRequestToLeg {
            leg_id: "b-1".into(),
            method: "INFO".into(),
            body: Some(Body::new(
                body.clone(),
                Some("application/example-binary".into()),
                BodyAuthor::Stack,
            )),
            headers: vec![
                ("User-To-User".into(), uui.into()),
                ("X-Example-Trace".into(), "abc-123".into()),
                // A body-owned header MUST be ignored — it is owned by
                // `content_type` and must never be emitted twice.
                ("Content-Type".into(), "text/bogus".into()),
            ],
        }],
    );
    assert_eq!(result.effects.outbound.len(), 1);
    let eff = &result.effects.outbound[0];
    assert_eq!(eff.leg_id.as_deref(), Some("b-1"));
    match &eff.body {
        OutboundBody::Request(r) => {
            assert_eq!(r.method(), "INFO");
            assert_eq!(&r.body()[..], &body[..], "opaque body passes through unchanged");
            // Forwarded application headers survive verbatim.
            assert_eq!(r.raw(HeaderName::from("user-to-user")).next(), Some(uui));
            assert_eq!(r.raw(HeaderName::from("x-example-trace")).next(), Some("abc-123"));
            // Content-Type is owned by `content_type`: exactly one, from the
            // body — NOT the bogus forwarded one (dedup guard).
            let cts: Vec<&str> = r.raw(HeaderName::ContentType).collect();
            assert_eq!(
                cts,
                vec!["application/example-binary"],
                "the body's Content-Type wins; a forwarded Content-Type is dropped, not duplicated"
            );
        }
        _ => panic!("expected an outbound request"),
    }
}

// 183 brokers an unadopted leg's SDP onto the a-leg as unreliable early media
// (RFC 3262 §3 / RFC 5009 P-Early-Media), minting the B2BUA's a-facing tag.
#[test]
fn send_provisional_to_leg_brokers_183_sdp_to_a() {
    let call = test_call();
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let sdp = b"v=0\r\no=mrf 1 1 IN IP4 10.0.0.50\r\n".to_vec();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 183,
            reason: "Session Progress".into(),
            body: Some(Body::new(sdp.clone(), None, BodyAuthor::Stack)),
            to_tag: None,
            p_early_media: Some("sendrecv".into()),
        }],
    );
    assert_eq!(result.effects.outbound.len(), 1);
    let eff = &result.effects.outbound[0];
    assert_eq!(eff.leg_id.as_deref(), Some("a"));
    match &eff.body {
        OutboundBody::Response(r) => {
            assert_eq!(r.status(), 183);
            assert_eq!(&r.body()[..], &sdp[..], "the MRF SDP is brokered onto A");
            assert!(r.to().tag().is_some(), "183 carries a B2BUA-minted early to-tag");
            assert_eq!(
                r.raw(HeaderName::ContentType).next(),
                Some("application/sdp"),
                "an SDP body defaults to application/sdp"
            );
            assert_eq!(r.raw(HeaderName::PEarlyMedia).next(), Some("sendrecv"));
        }
        _ => panic!("expected an outbound response"),
    }
    // The minted tag is persisted on the a-dialog for reuse on later 1xx.
    assert!(
        result.call.a_leg.own_tag.as_deref().is_some_and(|t| {
            !t.is_empty() && call::helpers::caller_dialog(&result.call, t).is_some()
        }),
        "the a-facing early tag is persisted"
    );
}

// A non-1xx status (or a non-a target) is rejected — no UAS transaction to
// answer.
#[test]
fn send_provisional_rejects_non_provisional_status() {
    let call = test_call();
    let event =
        CallEvent::sip(SipMessage::Request(in_dialog_info()), "10.0.0.2:5070".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "a",
        &config,
        &mut id_gen,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: None,
            p_early_media: None,
        }],
    );
    assert!(result.effects.outbound.is_empty(), "a non-1xx provisional is rejected");
}
