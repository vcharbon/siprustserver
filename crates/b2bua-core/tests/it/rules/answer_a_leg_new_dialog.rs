// ── AnswerALegNewDialog: a-side fork-confirm (RFC 3261 §12.1 / RFC 3264 §5.1) ──
//
// An early-media callflow answers ONE caller INVITE in two stages with two
// *different* To-tags: 183 (media-server SDP, tag A1) then 200 (callee SDP, tag
// A2 ≠ A1). `AnswerALegNewDialog` mints/adopts A2, supersedes the early a-dialog
// A1, relays the callee 200/SDP under A2, and confirms the a-leg.
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use b2bua_core::rules::RelayedFinal;
use sip_message::generators::SourceBody;
use sip_message::HeaderName;

/// The action ignores the triggering event (it operates on the call), so any
/// event drives it.
fn some_event() -> CallEvent {
    CallEvent::sip(
        SipMessage::Request(super::fixtures::invite()),
        "127.0.0.1:5060".parse().unwrap(),
    )
}

// The callee 200 answers the a-leg under a FRESH To-tag A2 ≠ the early A1:
// the a-dialog local_tag is re-stamped to A2, the SDP-B rides the 200, the
// answer SDP is cached for §13.3.1.4, and the a-leg is confirmed.
#[test]
fn answers_under_a_fresh_tag_superseding_the_early_dialog() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let sdp_b = b"v=0\r\no=callee 2 2 IN IP4 10.0.0.70\r\n".to_vec();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: Some(Body::from_leg(sdp_b.clone(), "b-1")),
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::none(),
        }],
    );
    assert_eq!(result.effects.outbound.len(), 1);
    let eff = &result.effects.outbound[0];
    assert_eq!(eff.leg_id.as_deref(), Some("a"));
    let a2 = match &eff.body {
        OutboundBody::Response(r) => {
            assert_eq!(r.status(), 200);
            assert_eq!(&r.body()[..], &sdp_b[..], "the callee SDP-B rides the 200");
            assert_eq!(
                r.raw(HeaderName::ContentType).next(),
                Some("application/sdp"),
                "an SDP body defaults to application/sdp"
            );
            let tag = r.to().tag().map(str::to_owned).expect("the 200 carries an a-facing To-tag");
            assert_ne!(tag, "A1early", "A2 ≠ the early-media tag A1 (RFC 3264 §5.1)");
            tag
        }
        _ => panic!("expected an outbound response"),
    };
    let d = result.call.a_leg.dialogs.confirmed().expect("a confirmed a-dialog");
    assert_eq!(d.our_tag, a2, "the confirmed a-dialog is A2's");
    assert!(
        result.call.a_leg.dialogs.by_tags("A1early", &d.peer_tag).is_some_and(|e| e.is_retired()),
        "the early-media dialog A1 is retired, not renamed"
    );
    assert_eq!(
        d.cached_sdp.as_deref(),
        Some(sdp_b.as_slice()),
        "the answer SDP is cached for a §13.3.1.4 un-ACKed-2xx retransmit"
    );
    assert_eq!(result.call.a_leg.state(), LegState::Confirmed, "the a-leg is confirmed");
}

/// Answering the caller ends the setup's reliable provisionals and nothing
/// else: a b-leg's re-INVITE 2xx still awaiting its ACK keeps its ladder,
/// its give-up and its retained marker (RFC 3261 §13.3.1.4; the RFC 6026
/// *Accepted* marker `reinvite-glare` reads), while the caller-facing §3
/// ladder is retired with the final (§17.2.1).
#[test]
fn answering_the_caller_leaves_a_b_legs_reinvite_2xx_ladder_alone() {
    let mut call = call_with_early_a_dialog("A1early");
    let mut b = super::fixtures::b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    let reinvite_2xx = call::Unacked2xx {
        dialog_tag: "svc".into(),
        cseq: 7,
        emission: call::RetainedEmission::paced(
            b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
            ("10.0.0.2".into(), 5070),
            sip_retransmit::Class::Final2xx,
            call::Repeated::response("INVITE", 200),
        )
        .0,
    };
    call::helpers::hold_answered_2xx(with_dialog(&mut b, "bobtag"), 7, reinvite_2xx);
    call::helpers::add_b_leg(&mut call, b);
    let reinvite_2xx =
        call::Obligation::AckOf2xx { leg: "b-1".into(), dialog_tag: "svc".into(), cseq: 7 };
    call.provisionals.push(call::testkit::shown_provisional(
        "b-1",
        "svc",
        1,
        9,
        "A1early",
        1,
        call::Ladder::Repeating(
            call::RetainedEmission::paced(
                b"SIP/2.0 183 Session Progress\r\n\r\n".to_vec(),
                ("10.0.0.2".into(), 5070),
                sip_retransmit::Class::ReliableProvisional,
                call::Repeated::response("INVITE", 183),
            )
            .0,
        ),
        call::ResponderAck::RelayedPrack,
    ));
    let provisional = call::Obligation::AwaitPrack { tag: "A1early".into(), rseq: call::RSeq(1) };
    for (key, fire_at) in [
        (TimerKey::obligation(TimerType::Rung, reinvite_2xx.clone()), 500),
        (TimerKey::obligation(TimerType::RepeatGiveUp, reinvite_2xx.clone()), 32_000),
        (TimerKey::obligation(TimerType::Rung, provisional.clone()), 500),
    ] {
        call::helpers::arm_timer(&mut call, key, fire_at);
    }

    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::none(),
        }],
    );

    let b_dialog = result.call.b_legs[0].dialogs.confirmed().expect("the b-leg dialog");
    assert!(
        call::helpers::answered_2xx(b_dialog).is_some(),
        "the b-leg's re-INVITE 2xx still awaits its ACK: marker kept",
    );
    let ladder_timers = |o: &call::Obligation| {
        result.call.timers.iter().filter(|t| t.key.obligation_of() == Some(o)).count()
    };
    assert_eq!(
        ladder_timers(&reinvite_2xx),
        2,
        "the b-leg 2xx's rung and give-up stay in the ledger"
    );
    assert_eq!(ladder_timers(&provisional), 0, "the caller-facing §3 ladder ends with the final");
    assert_eq!(
        result.call.provisionals[0].shown.as_ref().map(|s| &s.ladder),
        Some(&call::Ladder::Ceased(call::Ceased::Final)),
        "the provisional repeats nothing more",
    );
    let cancelled: Vec<String> = result
        .effects
        .critical
        .iter()
        .filter_map(|e| match e {
            CriticalStateEffect::CancelTimer { key } => Some(format!("{key:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec!["Rung on AwaitPrack:A1early:1"],
        "only the provisional's timers leave the driver",
    );
    let caller = result.call.a_leg.dialogs.confirmed();
    assert!(
        caller.and_then(call::helpers::answered_2xx).is_some(),
        "the caller's answer is retained under its own obligation",
    );
}

// An explicit `to_tag` is used verbatim; `header_updates` add non-structural
// headers (same discipline as `RespondToALeg`).
#[test]
fn honors_an_explicit_tag_and_header_updates() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: Some("A2explicit".into()),
            header_updates: vec![("X-Served-By".into(), Some("mrf".into()))],
            relayed: RelayedFinal::none(),
        }],
    );
    let eff = &result.effects.outbound[0];
    match &eff.body {
        OutboundBody::Response(r) => {
            assert_eq!(r.to().tag(), Some("A2explicit"), "the supplied A2 is used verbatim");
            assert_eq!(r.raw(HeaderName::from("x-served-by")).next(), Some("mrf"));
        }
        _ => panic!("expected an outbound response"),
    }
    assert_eq!(
        result.call.a_leg.dialogs.confirmed().map(|d| d.our_tag.as_str()),
        Some("A2explicit"),
    );
}

// A non-2xx status establishes no dialog — the primitive is a no-op (the
// abandoned early dialog / the ADR-0022 unanswered-a-leg funnel own failure).
#[test]
fn non_2xx_status_is_a_no_op() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 486,
            reason: "Busy Here".into(),
            body: None,
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::none(),
        }],
    );
    assert!(result.effects.outbound.is_empty(), "a non-2xx final establishes no a-dialog");
    let a1 = result.call.a_leg.dialogs.iter().map(|d| (d.our_tag.as_str(), &d.state));
    assert_eq!(
        a1.collect::<Vec<_>>(),
        vec![("A1early", &DialogState::Early)],
        "the early dialog A1 is left untouched",
    );
}

// The fork-confirm 2xx is an a-facing final the B2BUA answers on a leg of
// its own: with no callee response to relay an advertisement from and no
// declaration, it states none.
#[test]
fn states_no_capability_advert_unless_declared() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::none(),
        }],
    );
    match &result.effects.outbound[0].body {
        OutboundBody::Response(r) => {
            for name in [HeaderName::Allow, HeaderName::Supported, HeaderName::Accept] {
                assert_eq!(
                    r.raw(name.clone()).count(),
                    0,
                    "no {} on a 2xx nobody declared a set for",
                    name.as_wire_str()
                );
            }
        }
        _ => panic!("expected an outbound response"),
    }
}

// A header_updates entry naming Allow/Supported owns it: a set value
// replaces the resolved advert verbatim (exactly once), a removal keeps the header
// absent.
#[test]
fn header_updates_override_the_capability_advert() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: None,
            to_tag: None,
            header_updates: vec![
                ("Supported".into(), Some("timer".into())),
                ("Allow".into(), None),
            ],
            relayed: RelayedFinal::none(),
        }],
    );
    match &result.effects.outbound[0].body {
        OutboundBody::Response(r) => {
            assert_eq!(
                r.raw(HeaderName::Supported).next(),
                Some("timer"),
                "a set value replaces the resolved advert verbatim"
            );
            let n = r.raw(HeaderName::Supported).count();
            assert_eq!(n, 1, "the service value is not duplicated by the default");
            assert_eq!(
                r.raw(HeaderName::Allow).next(),
                None,
                "an explicit removal keeps the header absent"
            );
        }
        _ => panic!("expected an outbound response"),
    }
}
/// The callee final this answer delivers, stating privacy, an identity line
/// of its own, a vendor header, its capability advert and session interval,
/// and lines the stack owns.
fn callee_final() -> sip_message::SipResponse {
    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.70:5060;branch=z9hG4bK-b\r\n\
From: <sip:alice@host>;tag=alice-tag\r\n\
To: <sip:callee@host>;tag=callee-tag\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:callee@10.0.0.70:5060>\r\n\
Privacy: none\r\n\
X-Vendor-Id: 100000042\r\n\
X-Vendor-Thing: opaque-42\r\n\
Allow: INVITE, ACK, BYE\r\n\
Session-Expires: 1800;refresher=uas\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    }
}

/// RFC 3261 §16.6: the answer delivering a callee final carries that final's
/// relayable lines — and only those — the way the plain relay would: the
/// structural set and the callee's Contact stay behind, and the callee's
/// advert is the caller's to see where the face declares none.
#[test]
fn relays_the_delivered_final_onto_the_answer() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let delivered = callee_final();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: Some(Body::from_leg(delivered.body().to_vec(), "b-1")),
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::of(
                &delivered,
                &call.a_leg_invite,
                200,
                SourceBody::Verbatim,
                &B2buaConfig::default(),
            ),
        }],
    );
    match &result.effects.outbound[0].body {
        OutboundBody::Response(r) => {
            assert_eq!(r.raw(HeaderName::from("Privacy")).next(), Some("none"));
            assert_eq!(r.raw(HeaderName::from("X-Vendor-Id")).next(), Some("100000042"));
            assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("opaque-42"));
            assert_eq!(
                r.raw(HeaderName::Allow).next(),
                Some("INVITE, ACK, BYE"),
                "an undeclared half states what the delivered final advertised"
            );
            assert_eq!(r.raw(HeaderName::Allow).count(), 1);
            assert_eq!(
                r.raw(HeaderName::SessionExpires).next(),
                Some("1800;refresher=uas"),
                "the session-timer negotiation rides end to end (RFC 4028)"
            );
            assert_eq!(r.raw(HeaderName::Via).count(), 1, "the caller's own Via, not the callee's");
            assert_eq!(
                r.raw(HeaderName::Contact).count(),
                1,
                "the B2BUA's Contact, not the callee's"
            );
            assert_eq!(r.to().tag(), result.call.a_leg.own_tag.as_deref());
        }
        _ => panic!("expected an outbound response"),
    }
}

/// A `header_updates` entry naming a header owns it over the relayed line of
/// the same name — a set value replaces it, a removal keeps it absent.
#[test]
fn header_updates_own_a_name_over_the_relayed_line() {
    let call = call_with_early_a_dialog("A1early");
    let event = some_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let delivered = callee_final();
    let result = exec_on(
        &call,
        &event,
        "b-1",
        &config,
        &mut id_gen,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: Some(Body::from_leg(delivered.body().to_vec(), "b-1")),
            to_tag: None,
            header_updates: vec![
                ("X-Vendor-Thing".into(), Some("service-owned".into())),
                ("X-Vendor-Id".into(), None),
                ("Allow".into(), Some("INVITE".into())),
            ],
            relayed: RelayedFinal::of(
                &delivered,
                &call.a_leg_invite,
                200,
                SourceBody::Verbatim,
                &B2buaConfig::default(),
            ),
        }],
    );
    match &result.effects.outbound[0].body {
        OutboundBody::Response(r) => {
            assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("service-owned"));
            assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).count(), 1);
            assert_eq!(
                r.raw(HeaderName::from("X-Vendor-Id")).next(),
                None,
                "a removal keeps the relayed line off"
            );
            assert_eq!(r.raw(HeaderName::Allow).next(), Some("INVITE"));
            assert_eq!(r.raw(HeaderName::Allow).count(), 1);
            assert_eq!(
                r.raw(HeaderName::from("Privacy")).next(),
                Some("none"),
                "the rest still rides"
            );
        }
        _ => panic!("expected an outbound response"),
    }
}
