//! An UPDATE offer answered on the caller's face.

use super::fixtures::*;
use super::*;

/// The caller's early dialog `a1` (her INVITE offered) maps to b-2 after a
/// reroute; b-1's reliable 183 answered her offer there. Her UPDATE offer on
/// `a1` is legal on her face, but the stack's offer to b-2 stays open until b-2
/// itself answers reliably (RFC 3264 §4): 491, then nothing to refuse.
#[test]
fn an_update_offer_answered_on_the_caller_face_waits_on_the_current_peer() {
    let mut call = call_with_early_a_dialog("a1");
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    call.a_leg_invite
        .headers
        .push(call::SipHeader { name: "Content-Type".into(), value: "application/sdp".into() });
    let mut carol = b_leg_pending();
    call::testkit::stage_leg(&mut carol, LegState::Terminated);
    with_dialog(&mut carol, "c1");
    let mut bob = b_leg_pending();
    bob.leg_id = "b-2".into();
    call::testkit::stage_leg(&mut bob, LegState::Early);
    with_dialog(&mut bob, "b2");
    call::helpers::add_b_leg(&mut call, carol);
    call::helpers::add_b_leg(&mut call, bob);
    call.a_leg.dialogs.by_tags_mut("a1", &call.a_leg_invite.from_tag.clone()).unwrap().fronts =
        vec![PeerDialog { leg_id: "b-2".into(), peer_tag: "b2".into() }];
    let shown = |b_leg_id: &str, b_tag: &str| {
        let mut p = call::testkit::shown_provisional(
            b_leg_id,
            b_tag,
            1,
            1,
            "a1",
            1,
            call::Ladder::Acknowledged,
            call::ResponderAck::RelayedPrack,
        );
        p.responder_sdp = true;
        if let Some(s) = p.shown.as_mut() {
            s.carried_sdp = true;
        }
        p
    };
    call.provisionals.push(shown("b-1", "c1"));

    let raw = "UPDATE sip:b2bua@127.0.0.1:5080 SIP/2.0\r\n\
               Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKupd\r\n\
               From: <sip:alice@host>;tag=atag\r\nTo: <sip:bob@host>;tag=a1\r\n\
               Call-ID: c1@alice\r\nCSeq: 2 UPDATE\r\nMax-Forwards: 70\r\n\
               Content-Type: application/sdp\r\nContent-Length: 5\r\n\r\nv=0\r\n";
    let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a request")
    };
    // The gate resolves her UPDATE to her dialog `a1`.
    let resolved = call::helpers::Resolved {
        dialog: Some(call::DialogId { our_tag: "a1".into(), peer_tag: "atag".into() }),
        record: None,
    };
    let config = B2buaConfig::default();
    let refusal = |call: &call::Call| {
        let event =
            CallEvent::sip(SipMessage::Request(req.clone()), "127.0.0.1:5060".parse().unwrap());
        let ctx = context(call, &event, "a", &config).resolving(&resolved);
        ctx.update_refusal()
    };
    assert_eq!(refusal(&call), Some(b2bua_sdk::model::GlareRefusal::Crossing));
    call.provisionals.push(shown("b-2", "b2"));
    assert_eq!(refusal(&call), None, "b-2 answered reliably: the offer relays");
}
