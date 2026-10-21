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

use super::lost_round_rules::{bridged, requests_to, turn};

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

/// The callee's `status` final to the `method` of CSeq `cseq` on its dialog,
/// carrying `sdp`.
fn from_bob(status: u16, cseq: u32, method: &str, sdp: &str) -> CallEvent {
    let typed = if sdp.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let raw = format!(
        "SIP/2.0 {status} OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK{method}{cseq}\r\n\
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
    CallEvent::Sip {
        message: Box::new(SipMessage::Response(resp)),
        src: "10.0.0.2:5070".parse().unwrap(),
        matched_client_txn: true,
    }
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
    let (rule, result) = turn(&call, &from_bob(200, reinvite, "INVITE", OFFER));
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
    let (_, answered) = turn(&call, &from_bob(200, bye, "BYE", ""));
    let call = answered.expect("the BYE's final resolves the leg").call;
    let (rule, result) = turn(&call, &from_bob(200, reinvite, "INVITE", OFFER));
    assert_eq!(rule, Some("ack-2xx-on-ended-dialog"));
    let (cseq, answer) = ack_to_bob(&result.expect("a turn"));
    assert_eq!(cseq, reinvite);
    assert!(answer.contains("m=audio 0 "), "{answer}");
}

#[test]
fn a_repeat_of_that_2xx_is_re_acked_with_the_same_ack() {
    let (call, reinvite, _) = reinvite_then_bye();
    let event = from_bob(200, reinvite, "INVITE", OFFER);
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
    let (rule, _) = turn(&reinvited.call, &from_bob(200, cseq, "INVITE", OFFER));
    assert_ne!(rule, Some("ack-2xx-on-ended-dialog"));
}

#[test]
fn a_2xx_answering_our_offer_is_acked_bare() {
    let ours = Body::own(OFFER.replace("bob", "svc").into_bytes(), None);
    let (call, reinvite, _) = reinvite_carrying_then_bye(Some(ours));
    let (rule, result) = turn(&call, &from_bob(200, reinvite, "INVITE", OFFER));
    assert_eq!(rule, Some("ack-2xx-on-ended-dialog"));
    let (_, answer) = ack_to_bob(&result.expect("a turn"));
    assert!(answer.is_empty(), "the 2xx answered our offer: the ACK is bare: {answer}");
}

#[test]
fn a_2xx_whose_relayed_ack_is_still_owed_is_left_to_its_originator() {
    let (mut call, reinvite, _) = reinvite_then_bye();
    let leg = call.b_legs.iter_mut().find(|l| l.leg_id == "b-1").expect("the callee's leg");
    let dialog = leg.dialogs.established_mut().expect("the callee's dialog");
    dialog.awaited_ack_cseq = Some(i64::from(reinvite));
    let (rule, _) = turn(&call, &from_bob(200, reinvite, "INVITE", OFFER));
    assert_ne!(rule, Some("ack-2xx-on-ended-dialog"));
}
