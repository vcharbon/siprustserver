//! A 2xx to this stack's own re-INVITE that reaches it after a BYE ended the
//! dialog, or while that BYE awaits its final (RFC 3261 §13.2.2.4, RFC 5407 §2):
//! it is ACKed under the re-INVITE's CSeq, an offer it makes answered with every
//! stream rejected (RFC 3264 §6), and draws no second BYE. A repeat is re-ACKed
//! with the same ACK. A 2xx whose ACK a relayed INVITE's originator still owes
//! is not this stack's to acknowledge.

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{HandlerResult, OutboundBody};
use b2bua_core::rules::{ActionExecutor, RuleAction, RuleCall, RuleContext};
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::Body;
use call::Direction;
use sip_ids::TurnIds;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::evidence_rules::{bridged, requests_to, turn};

/// The callee's offer in a 2xx to an offerless re-INVITE.
const OFFER: &str = "v=0\r\no=bob 2 2 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=sendrecv\r\n";

/// `actions` executed on `call` in one turn.
fn executed(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let config = B2buaConfig::default();
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
    };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut ids = TurnScope::of(TurnIds::fixed(2));
    ActionExecutor { config: &config, turn: &mut ids, now_ms: 0 }.execute(actions, call, &ctx)
}

/// The branch of the request of CSeq `cseq` the record holds as sent on
/// `leg`'s dialog: the one its final names.
fn sent_branch(call: &call::Call, leg: &str, cseq: u32) -> String {
    let leg = call::helpers::find_leg(call, leg).expect("the leg");
    let cseq = i64::from(cseq);
    leg.dialogs
        .iter()
        .flat_map(|d| d.sent_invite.iter().chain(d.requests.iter()))
        .find(|r| r.outbound_cseq == cseq)
        .and_then(|r| r.branch().map(str::to_string))
        .expect("a request the record holds at that number")
}

/// The callee's `status` final to the `method` of CSeq `cseq` this stack sent
/// on its dialog of `call`, carrying `sdp`.
fn from_bob(call: &call::Call, status: u16, cseq: u32, method: &str, sdp: &str) -> CallEvent {
    let branch = sent_branch(call, "b-1", cseq);
    let typed = if sdp.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let raw = format!(
        "SIP/2.0 {status} OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch={branch}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
{typed}Content-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a response")
    };
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// The requests `result` sends toward the callee.
fn sent_to_bob(result: &HandlerResult) -> Vec<SipRequest> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("b-1"))
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// The bridged call after an offerless re-INVITE toward the callee, then the
/// BYE ending its dialog: the call, the re-INVITE's CSeq and the BYE's.
fn reinvite_then_bye() -> (call::Call, u32, u32) {
    reinvite_carrying_then_bye(None)
}

/// [`reinvite_then_bye`] with the re-INVITE carrying `offer`.
fn reinvite_carrying_then_bye(offer: Option<Body>) -> (call::Call, u32, u32) {
    let offered = offer.is_some();
    let reinvited = executed(
        &bridged(),
        &[RuleAction::SendReinvite { leg_id: "b-1".into(), body: offer, add_headers: vec![] }],
    );
    let reinvite = sent_to_bob(&reinvited).remove(0);
    assert_eq!(!reinvite.body().is_empty(), offered, "the re-INVITE's offer, if any");
    let ended = executed(
        &reinvited.call,
        &[RuleAction::DestroyLeg { leg_id: "b-1".into(), headers: vec![] }],
    );
    let bye = sent_to_bob(&ended).remove(0);
    assert_eq!(bye.method().as_str(), "BYE");
    (ended.call, reinvite.cseq().seq(), bye.cseq().seq())
}

/// The ACK `result` sends the callee, with its CSeq and body.
fn ack_to_bob(result: &HandlerResult) -> (u32, String) {
    let acks: Vec<_> =
        sent_to_bob(result).into_iter().filter(|r| r.method().as_str() == "ACK").collect();
    assert_eq!(acks.len(), 1, "one ACK: {:?}", requests_to(result, "b-1"));
    (acks[0].cseq().seq(), String::from_utf8_lossy(acks[0].body()).to_string())
}

#[test]
fn a_2xx_crossing_our_bye_is_acked_its_offer_rejected() {
    let (call, reinvite, _) = reinvite_then_bye();
    let (rule, result) = turn(&call, &from_bob(&call, 200, reinvite, "INVITE", OFFER));
    assert_eq!(rule, Some("ack-2xx-on-ended-dialog"));
    let result = result.expect("a turn");
    let (cseq, answer) = ack_to_bob(&result);
    assert_eq!(cseq, reinvite, "the ACK names the re-INVITE");
    assert!(answer.contains("m=audio 0 "), "the offer answered, its stream rejected: {answer}");
    assert!(
        !requests_to(&result, "b-1").iter().any(|(m, _)| m == "BYE"),
        "the dialog's BYE is already sent"
    );
}

#[test]
fn a_2xx_after_our_bye_was_answered_is_acked() {
    let (call, reinvite, bye) = reinvite_then_bye();
    let (_, answered) = turn(&call, &from_bob(&call, 200, bye, "BYE", ""));
    let call = answered.expect("the BYE's final resolves the leg").call;
    let (rule, result) = turn(&call, &from_bob(&call, 200, reinvite, "INVITE", OFFER));
    assert_eq!(rule, Some("ack-2xx-on-ended-dialog"));
    let (cseq, answer) = ack_to_bob(&result.expect("a turn"));
    assert_eq!(cseq, reinvite);
    assert!(answer.contains("m=audio 0 "), "{answer}");
}

#[test]
fn a_repeat_of_that_2xx_is_re_acked_with_the_same_ack() {
    let (call, reinvite, _) = reinvite_then_bye();
    let event = from_bob(&call, 200, reinvite, "INVITE", OFFER);
    let (_, first) = turn(&call, &event);
    let first = first.expect("a turn");
    let (rule, _) = turn(&first.call, &event);
    assert_eq!(rule, Some("re-ack-retransmitted-2xx"));
}

#[test]
fn a_2xx_on_a_confirmed_dialog_is_not_this_rules() {
    let reinvited = executed(
        &bridged(),
        &[RuleAction::SendReinvite { leg_id: "b-1".into(), body: None, add_headers: vec![] }],
    );
    let cseq = sent_to_bob(&reinvited).remove(0).cseq().seq();
    let (rule, _) = turn(&reinvited.call, &from_bob(&reinvited.call, 200, cseq, "INVITE", OFFER));
    assert_ne!(rule, Some("ack-2xx-on-ended-dialog"));
}

#[test]
fn a_2xx_answering_our_offer_is_acked_bare() {
    let ours = Body::own(OFFER.replace("bob", "svc").into_bytes(), None);
    let (call, reinvite, _) = reinvite_carrying_then_bye(Some(ours));
    let (rule, result) = turn(&call, &from_bob(&call, 200, reinvite, "INVITE", OFFER));
    assert_eq!(rule, Some("ack-2xx-on-ended-dialog"));
    let (_, answer) = ack_to_bob(&result.expect("a turn"));
    assert!(answer.is_empty(), "the 2xx answered our offer: the ACK is bare: {answer}");
}

#[test]
fn a_2xx_whose_relayed_ack_is_still_owed_is_left_to_its_originator() {
    let (mut call, reinvite, _) = reinvite_then_bye();
    let leg = call.b_legs.iter_mut().find(|l| l.leg_id == "b-1").expect("the callee's leg");
    let dialog = leg.dialogs.established_mut().expect("the callee's dialog");
    call::helpers::sent_exchange_mut(dialog).expect("the re-INVITE's record").awaited_ack_cseq =
        Some(i64::from(reinvite));
    let (rule, _) = turn(&call, &from_bob(&call, 200, reinvite, "INVITE", OFFER));
    assert_ne!(rule, Some("ack-2xx-on-ended-dialog"));
}

/// The answer the stack composes continues the session it stated on that
/// dialog (RFC 3264 §8): the version after the one its INVITE stated, under
/// the same `o=` identity, never a session of its own minted for the ACK.
#[test]
fn the_rejecting_answer_continues_the_session_the_stack_stated() {
    let (mut call, reinvite, _) = reinvite_then_bye();
    let leg = call.b_legs.iter_mut().find(|l| l.leg_id == "b-1").expect("the callee's leg");
    leg.sdp_session.sent_origin = Some("- 708 708 IN IP4 10.10.0.9".into());
    leg.sdp_session.sent_media = vec!["audio 4000 RTP/AVP 0".into()];
    let (_, result) = turn(&call, &from_bob(&call, 200, reinvite, "INVITE", OFFER));
    let (_, answer) = ack_to_bob(&result.expect("a turn"));
    assert!(answer.contains("\r\no=- 708 709 IN IP4 10.10.0.9\r\n"), "{answer}");
    assert!(answer.contains("m=audio 0 "), "{answer}");
}

/// The caller's `status` final to the re-INVITE of CSeq `cseq` this stack sent
/// on her dialog, carrying `sdp`.
fn from_alice(call: &call::Call, status: u16, cseq: u32, sdp: &str) -> CallEvent {
    let branch = sent_branch(call, "a", cseq);
    let raw = format!(
        "SIP/2.0 {status} OK\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
From: <sip:bob@host>;tag=b2bua-a\r\n\
To: <sip:alice@host>;tag=atag\r\n\
Call-ID: c1@alice\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:alice@127.0.0.1:5060>\r\n\
Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a response")
    };
    CallEvent::sip(SipMessage::Response(resp), "127.0.0.1:5060".parse().unwrap())
}

/// The same on the caller's leg: the stack's offerless re-INVITE toward her
/// crosses her BYE, and the offer her 2xx makes is answered in the session
/// the stack stated on her dialog, never one minted for the ACK.
#[test]
fn on_the_callers_leg_the_rejecting_answer_continues_the_stacks_session() {
    let reinvited = executed(
        &bridged(),
        &[RuleAction::SendReinvite { leg_id: "a".into(), body: None, add_headers: vec![] }],
    );
    let mut call = reinvited.call;
    let cseq = call.a_leg.dialogs.confirmed().expect("her dialog").our_cseq;
    call.a_leg.sdp_session.sent_origin = Some("bob 2 2 IN IP4 10.0.0.2".into());
    call.a_leg.sdp_session.sent_media = vec!["audio 4000 RTP/AVP 0".into()];
    call::testkit::stage_leg(&mut call.a_leg, call::LegState::Terminated);
    let config = B2buaConfig::default();
    let event = from_alice(
        &call,
        200,
        u32::try_from(cseq).unwrap(),
        OFFER.replace("bob 2 2", "alice 9 9").as_str(),
    );
    let resolved = call::helpers::Resolved {
        dialog: Some(call::DialogId { our_tag: "b2bua-a".into(), peer_tag: "atag".into() }),
        record: None,
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &resolved,
    };
    let rules = b2bua_core::rules::default_rules();
    let rule = b2bua_core::rules::pick_ranked(&rules, &call, &ctx).into_iter().next();
    assert_eq!(rule.map(|r| r.id), Some("ack-2xx-on-ended-dialog"));
    let actions = (rule.expect("the rule").handle)(&ctx).map(|r| r.actions).unwrap_or_default();
    let mut ids = TurnScope::of(TurnIds::fixed(3));
    let result = ActionExecutor { config: &config, turn: &mut ids, now_ms: 0 }
        .execute(&actions, &call, &ctx);
    let ack = requests_to(&result, "a");
    assert!(ack.iter().any(|(m, _)| m == "ACK"), "{ack:?}");
    let body: Vec<String> = result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "ACK" => {
                Some(String::from_utf8_lossy(r.body()).to_string())
            }
            _ => None,
        })
        .collect();
    assert!(body[0].contains("\r\no=bob 2 3 IN IP4 10.0.0.2\r\n"), "{body:?}");
}
