//! A BYE against the ACK of a 2xx: a peer's BYE answered by a rule that ends
//! the peer's leg first draws the ACK of the 2xx that peer answered this
//! stack's INVITE with (RFC 3261 §13.2.2.4), whichever rule answers it; and a
//! BYE this stack sends waits for the ACK of its own 2xx on the dialog (§15),
//! whichever action sends it.

use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;

/// A re-INVITE this stack relayed to bob carrying an offer, CSeq 2.
const REINVITE: &str = "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-reinvite\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\nTo: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: b-1@svc\r\nCSeq: 2 INVITE\r\nMax-Forwards: 70\r\nContact: <sip:svc@10.0.0.9>\r\n\
Content-Type: application/sdp\r\nContent-Length: 74\r\n\r\n\
v=0\r\no=- 1 2 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\n\
m=audio 4000 RTP/AVP 0\r\n";

/// The call with bob's confirmed leg `b-1` holding that re-INVITE's 2xx,
/// its ACK awaited and composable by this stack alone: the re-INVITE carried
/// the offer.
fn bob_owed_a_bare_ack() -> call::Call {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    let destination = call::HostPort { host: "10.0.0.2".into(), port: 5070 };
    let image = REINVITE.as_bytes().to_vec();
    let sent = call::SentRequest { branch: "z9hG4bK-reinvite".into(), image, destination };
    let mut reinvite =
        call::RequestRecord { sent: Some(sent), ..call::RequestRecord::relayed("INVITE", 2, 2) };
    if let Some(x) = reinvite.invite.as_mut() {
        x.awaited_ack_cseq = Some(2);
    }
    with_dialog(&mut b, "bobtag").sent_invite = Some(reinvite);
    call::helpers::add_b_leg(call, b)
}

/// bob's BYE answered 200 by a rule ending his leg — not `relay-bye`'s own
/// list — draws his ACK first.
#[test]
fn a_peers_bye_answered_by_ending_its_leg_draws_its_ack_first() {
    let call = bob_owed_a_bare_ack();
    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::sip(SipMessage::Request(bye), "10.0.0.2:5070".parse().unwrap());
    let actions = [
        RuleAction::Respond { status: 200, reason: "OK".into(), body: vec![], content_type: None },
        RuleAction::TerminateLeg { leg_id: "b-1".into(), end: LegEnd::PeerByeAnswered },
    ];
    let result = execute_from_b1(&call, &event, &actions);
    let wire: Vec<String> = result
        .effects
        .outbound
        .iter()
        .map(|e| match &e.body {
            OutboundBody::Request(r) => r.method().as_str().to_string(),
            OutboundBody::Response(r) => format!("{} {}", r.status(), r.cseq().method()),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(wire, ["ACK", "200 BYE"], "the ACK of bob's 2xx leaves before the 200 to his BYE");
}

/// A service destroying the caller's leg while her 2xx awaits its ACK: the
/// BYE waits behind that ACK, the 2xx's ladder still running (RFC 3261 §15).
#[test]
fn a_destroyed_callers_leg_byes_her_after_her_ack() {
    let mut call = call_with_retained_answered_2xx();
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    let result =
        execute_on(&call, &[RuleAction::DestroyLeg { leg_id: "a".into(), headers: vec![] }]);
    let byes = result.effects.outbound.iter().filter(
        |e| matches!(&e.body, OutboundBody::Request(r) if *r.method() == sip_message::Method::Bye),
    );
    assert_eq!(byes.count(), 0, "no BYE reaches her before her ACK");
    let held = result.call.a_leg.dialogs.iter().find_map(|d| d.answered_invite.as_ref());
    let held = held.and_then(|r| r.invite.as_ref()).and_then(|x| x.bye_behind_ack.as_ref());
    assert!(held.is_some(), "the BYE waits on her 2xx's record");
    let ladder = TimerKey::obligation(TimerType::RepeatGiveUp, answered_obligation());
    assert!(result.call.timers.iter().any(|t| t.key == ladder), "her 2xx's give-up stays armed");
}

/// A service's own BYE to the caller (`SendRequestToLeg`) while her 2xx
/// awaits its ACK takes the BYE row of the method table: it waits behind
/// that ACK as the stack's own BYE does (RFC 3261 §15).
#[test]
fn a_services_bye_to_the_caller_waits_for_her_ack() {
    let mut call = call_with_retained_answered_2xx();
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    let bye = RuleAction::SendRequestToLeg {
        leg_id: "a".into(),
        method: "BYE".into(),
        body: None,
        headers: vec![("Reason".into(), "Q.850;cause=16".into())],
    };
    let result = execute_on(&call, &[bye]);
    let byes = result.effects.outbound.iter().filter(
        |e| matches!(&e.body, OutboundBody::Request(r) if *r.method() == sip_message::Method::Bye),
    );
    assert_eq!(byes.count(), 0, "no BYE reaches her before her ACK");
    let held = result.call.a_leg.dialogs.iter().find_map(|d| d.answered_invite.as_ref());
    let held = held.and_then(|r| r.invite.as_ref()).and_then(|x| x.bye_behind_ack.as_ref());
    let Some(call::QueuedRequest::Bye { headers, .. }) = held else {
        panic!("the BYE waits on her 2xx's record: {held:?}");
    };
    assert!(
        headers.iter().any(|(name, value)| name == "Reason" && value == "Q.850;cause=16"),
        "the service's lines ride the held BYE: {headers:?}"
    );
}

/// The same BYE followed, as every service sends it, by the leg's end as
/// this stack's BYE sent: the BYE still waits behind her 2xx, whose ladder
/// stays armed, and leaves on her ACK (RFC 3261 §15).
#[test]
fn a_services_bye_ending_the_callers_leg_leaves_on_her_ack() {
    let mut call = call_with_retained_answered_2xx();
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    let actions = [
        RuleAction::SendRequestToLeg {
            leg_id: "a".into(),
            method: "BYE".into(),
            body: None,
            headers: vec![],
        },
        RuleAction::TerminateLeg { leg_id: "a".into(), end: LegEnd::OurByeSent },
    ];
    let result = execute_on(&call, &actions);
    let held = result.call.a_leg.dialogs.iter().find_map(|d| d.answered_invite.as_ref());
    let held = held.and_then(|r| r.invite.as_ref()).and_then(|x| x.bye_behind_ack.as_ref());
    assert!(held.is_some(), "the BYE waits on her 2xx's record");
    let ladder = TimerKey::obligation(TimerType::RepeatGiveUp, answered_obligation());
    assert!(result.call.timers.iter().any(|t| t.key == ladder), "her 2xx's give-up stays armed");

    let (_, _, fx) = super::fixtures::discharge_ack(&result.call, "a");
    let byes = fx.outbound.iter().filter(
        |e| matches!(&e.body, OutboundBody::Request(r) if *r.method() == sip_message::Method::Bye),
    );
    assert_eq!(byes.count(), 1, "her ACK releases the BYE");
}
