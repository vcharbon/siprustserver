//! An INVITE final on a confirmed dialog, as the default rules read it. One
//! whose CSeq is above the last the record sent on the dialog, no request
//! record answering it, is evidence the record cannot explain (ADR-0055 §4):
//! it is ACKed — a 2xx end to end under a raised sequence (RFC 3261
//! §13.2.2.4), a non-2xx hop by hop on its own branch (§17.1.1.3) — and the
//! call is ended. A repeat of that 2xx is re-ACKed with the same ACK. A
//! non-2xx that is no evidence is not read as the confirmed leg failing.

use std::net::SocketAddr;

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{HandlerResult, OutboundBody};
use b2bua_core::initial_invite::build_initial_call;
use b2bua_core::rules::{
    default_rules, pick_ranked, ActionExecutor, RuleAction, RuleCall, RuleContext,
};
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use call::{
    Dialog, DialogId, DialogState, Direction, Leg, LegKind, LegState, OutboundInvite, RemoteInfo,
};
use sip_ids::TurnIds;
use sip_message::generators::OutOfDialogMethod;
use sip_message::generators::{generate_out_of_dialog_request, GenerateOutOfDialogRequestOpts};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipStr};

/// The CSeq the record last sent on the callee's dialog.
const RECORDED: u32 = 3;

fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

/// A call bridged to `b-1`: the caller's dialog `atag`, the callee's `svc`/`bobtag`,
/// the callee's sequence at [`RECORDED`].
pub(super) fn bridged() -> call::Call {
    let invite = generate_out_of_dialog_request(
        OutOfDialogMethod::Invite,
        &GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
            call_id: "c1@alice".into(),
            from: Some(
                header::From::from_uri(uri_of("sip:alice@host"))
                    .with_tag(SipStr::from_static("atag")),
            ),
            to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
            cseq: 1,
            via: Some(Via::udp("127.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKa"))),
            contact: Some(header::Contact::from_uri(uri_of("sip:alice@127.0.0.1:5060"))),
            max_forwards: Some(70),
            body: Vec::new(),
            content_type: None,
            extra_headers: vec![],
        },
    )
    .expect("the caller's INVITE");
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    let mut call =
        build_initial_call(&invite, src, &B2buaConfig::default(), &mut TurnIds::fixed(1), 0);
    let mut a = Dialog::new("b2bua-a", "atag", DialogState::Confirmed, 1);
    a.peer_cseq = Some(1);
    a.remote_target = "sip:alice@127.0.0.1:5060".into();
    call.a_leg.dialogs.insert(a);
    call.a_leg.own_tag = Some("b2bua-a".into());
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    let mut b = Leg {
        leg_id: "b-1".into(),
        call_id: "bcid@x".into(),
        source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
        dialogs: Default::default(),
        local_uri: Some("sip:svc@10.0.0.9".into()),
        remote_uri: Some("sip:bob@10.0.0.2".into()),
        contact: "sip:b2bua@127.0.0.1:5060".into(),
        invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
        invite: Some(call::RequestRecord::dialled(1, None, OutboundInvite::new("svc", 0))),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        adopted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    };
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    b.dialogs.retain(|d| d.peer_tag != "staged-peer");
    let mut d = Dialog::new("svc", "bobtag", DialogState::Confirmed, i64::from(RECORDED));
    d.remote_target = "sip:bob@10.0.0.2:5070".into();
    b.dialogs.insert(d);
    call::helpers::add_b_leg(call, b)
}

/// The callee's final `status` to an INVITE of CSeq `cseq` on its dialog.
fn final_from_bob(status: u16, cseq: u32) -> CallEvent {
    final_from_bob_carrying(status, cseq, "")
}

/// [`final_from_bob`] carrying `sdp` as its body.
fn final_from_bob_carrying(status: u16, cseq: u32, sdp: &str) -> CallEvent {
    let typed = if sdp.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let raw = format!(
        "SIP/2.0 {status} Whatever\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKlost{cseq}\r\n\
From: <sip:svc@10.0.0.9>;tag=svc\r\n\
To: <sip:bob@10.0.0.2>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
{typed}Content-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a response")
    };
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// The callee's dialog, as the engine resolves a response on it.
fn on_bobs_dialog() -> call::helpers::Resolved {
    let dialog = DialogId { our_tag: "svc".into(), peer_tag: "bobtag".into() };
    call::helpers::Resolved { dialog: Some(dialog), record: None }
}

/// The rule that claims `event` from `b-1`, and the turn it makes.
pub(super) fn turn(
    call: &call::Call,
    event: &CallEvent,
) -> (Option<&'static str>, Option<HandlerResult>) {
    let config = B2buaConfig::default();
    let resolved = on_bobs_dialog();
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &resolved,
    };
    // The evidence stage reads the event before the rule chain does.
    let evidence = b2bua_core::rules::evidence::evidence_rules(false);
    let rules = default_rules();
    let first = pick_ranked(&evidence, call, &ctx).into_iter().next();
    let Some(rule) = first.or_else(|| pick_ranked(&rules, call, &ctx).into_iter().next()) else {
        return (None, None);
    };
    let actions: Vec<RuleAction> = (rule.handle)(&ctx).map(|r| r.actions).unwrap_or_default();
    let mut id_gen = TurnScope::of(TurnIds::fixed(1));
    let mut exec = ActionExecutor {
        owners: b2bua_core::owners::Owners::none(),
        config: &config,
        turn: &mut id_gen,
        now_ms: 0,
    };
    (Some(rule.id), Some(exec.execute(&actions, call, &ctx)))
}

/// The requests `result` sends toward `leg`: method and CSeq number.
pub(super) fn requests_to(result: &HandlerResult, leg: &str) -> Vec<(String, u32)> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) => Some((r.method().as_str().to_string(), r.cseq().seq())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_2xx_beyond_the_record_is_acked_under_its_cseq_and_the_call_ended() {
    let call = bridged();
    let (rule, result) = turn(&call, &final_from_bob(200, RECORDED + 2));
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let to_bob = requests_to(&result, "b-1");
    assert_eq!(to_bob[0], ("ACK".into(), RECORDED + 2), "the ACK names the round");
    assert!(
        to_bob.iter().any(|(m, cseq)| m == "BYE" && *cseq > RECORDED + 2),
        "the callee's BYE is numbered past the round: {to_bob:?}",
    );
    assert!(requests_to(&result, "a").iter().any(|(m, _)| m == "BYE"), "the caller's BYE");
    assert_eq!(result.call.state, call::CallModelState::Terminating);
}

#[test]
fn a_repeat_of_an_evidence_2xx_is_re_acked_with_the_same_ack() {
    let call = bridged();
    let event = final_from_bob(200, RECORDED + 2);
    let (_, first) = turn(&call, &event);
    let first = first.expect("a turn");
    let ack = |r: &HandlerResult| {
        r.effects
            .outbound
            .iter()
            .find_map(|e| match &e.body {
                OutboundBody::Request(r) if r.method().as_str() == "ACK" => {
                    Some(r.image().to_vec())
                }
                OutboundBody::Datagram(d) => Some(d.wire().0.to_vec()),
                _ => None,
            })
            .expect("an ACK")
    };
    let (rule, repeat) = turn(&first.call, &event);
    assert_eq!(rule, Some("re-ack-retransmitted-2xx"));
    assert_eq!(ack(&repeat.expect("a turn")), ack(&first), "the same ACK (§13.2.2.4)");
}

#[test]
fn a_non_2xx_beyond_the_record_is_acked_on_its_own_branch_and_the_call_ended() {
    let call = bridged();
    let (rule, result) = turn(&call, &final_from_bob(488, RECORDED + 1));
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let ack = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "ACK" => Some(r.clone()),
            _ => None,
        })
        .expect("the ACK");
    assert_eq!(ack.top_via().branch(), Some("z9hG4bKlost4"), "the response's own branch");
    assert_eq!(ack.cseq().seq(), RECORDED + 1);
    assert_eq!(ack.request_uri().text(), "sip:bob@10.0.0.2:5070", "where the INVITE went");
    assert_eq!(result.call.state, call::CallModelState::Terminating);
}

#[test]
fn a_non_2xx_that_is_no_evidence_is_not_the_confirmed_leg_failing() {
    let call = bridged();
    let (rule, _) = turn(&call, &final_from_bob(488, RECORDED));
    assert_ne!(rule, Some("route-failure"), "a confirmed leg is not dialling");
    assert_eq!(rule, None, "no rule claims it");
}

/// The INVITE carried no offer (a re-INVITE the record never
/// held), and the callee's 2xx makes one: the ACK owes the answer
/// (RFC 3261 §13.2.2.4, RFC 3264 §4), and this stack answers it rejecting every
/// stream (RFC 3264 §6) before the BYE.
#[test]
fn an_offer_in_an_evidence_2xx_is_answered_in_the_ack_every_stream_rejected() {
    const OFFER: &str = "v=0\r\no=bob 7 7 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
    let call = bridged();
    let (rule, result) = turn(&call, &final_from_bob_carrying(200, RECORDED + 2, OFFER));
    assert_eq!(rule, Some("evidence-final"));
    let result = result.expect("a turn");
    let ack = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method().as_str() == "ACK" => Some(r.clone()),
            _ => None,
        })
        .expect("the ACK");
    let answer = String::from_utf8_lossy(ack.body()).to_string();
    assert!(answer.contains("m=audio 0 "), "every stream rejected: {answer}");
    assert!(requests_to(&result, "b-1").iter().any(|(m, _)| m == "BYE"), "then the BYE");
}

/// The callee repeats its non-2xx while the ACK has not reached it (RFC 3261
/// §17.2.1): no transaction here absorbs the repeat, so the record's own ACK is
/// sent again, the same datagram (§17.1.1.2).
#[test]
fn a_repeat_of_an_evidence_non_2xx_is_re_acked_with_the_same_ack() {
    let call = bridged();
    let event = final_from_bob(488, RECORDED + 1);
    let (_, first) = turn(&call, &event);
    let first = first.expect("a turn");
    let ack = |r: &HandlerResult| {
        r.effects
            .outbound
            .iter()
            .find_map(|e| match &e.body {
                OutboundBody::Request(r) if r.method().as_str() == "ACK" => {
                    Some(r.image().to_vec())
                }
                OutboundBody::Datagram(d) => Some(d.wire().0.to_vec()),
                _ => None,
            })
            .expect("an ACK")
    };
    let (rule, repeat) = turn(&first.call, &event);
    assert_eq!(rule, Some("re-ack-evidence"));
    assert_eq!(ack(&repeat.expect("a turn")), ack(&first), "the same ACK");
}
