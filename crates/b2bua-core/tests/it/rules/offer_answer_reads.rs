//! The offer/answer readings of the action primitives, as `sdp()` reads the
//! body: an answer cached for the caller's dialog is the description alone,
//! wherever the body framed it; the via-extension's offer flag on the stack's
//! own in-dialog INVITE reads its description; and an ACK given another
//! leg's answer carries it only where the dialog's 2xx made an offer (RFC
//! 3264 §4).

use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use b2bua_core::rules::RelayedFinal;
use sip_ids::SigningKey;

const SDP: &str = "v=0\r\no=bob 2 2 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=alice 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const MULTIPART: &str = "multipart/mixed;boundary=B";

/// `SDP` framed beside a text part.
fn framed() -> String {
    format!(
        "--B\r\nContent-Type: application/sdp\r\n\r\n{SDP}\r\n\
         --B\r\nContent-Type: text/plain\r\n\r\na note\r\n--B--\r\n"
    )
}

/// Bob's 200 to the b-leg INVITE, `body` typed `content_type`.
fn bobs_200(content_type: &str, body: &str) -> CallEvent {
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Type: {content_type}\r\n\
Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// A call whose pending b-leg was dialled with `invite`.
fn dialled_with(invite: &SipRequest) -> call::Call {
    let mut call = test_call();
    let mut leg = b_leg_pending();
    call::testkit::dial_with(
        &mut leg,
        call::SentRequest {
            branch: "z9hG4bKb".into(),
            image: invite.image().to_vec(),
            destination: call::HostPort { host: "10.0.0.2".into(), port: 5070 },
        },
    );
    call::helpers::add_b_leg(&mut call, leg);
    call
}

/// `actions` run on `call` for `event` arriving on the b-leg.
fn on_b(call: &call::Call, event: &CallEvent, actions: &[RuleAction]) -> HandlerResult {
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    exec_on(call, event, "b-1", &config, &mut id_gen, actions)
}

/// The requests of `method` the turn sent.
fn sent(result: &HandlerResult, method: Method) -> Vec<SipRequest> {
    result
        .effects
        .outbound
        .iter()
        .filter_map(|e| match &e.body {
            OutboundBody::Request(r) if r.method() == method => Some(r.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_confirmed_2xx_caches_its_description_alone() {
    let call = dialled_with(&invite());
    let event = bobs_200(MULTIPART, &framed());
    let result = on_b(&call, &event, &[RuleAction::ConfirmDialog { leg_id: "b-1".into() }]);
    let cached = call::helpers::own_dialog(&result.call.a_leg).and_then(|d| d.cached_sdp.clone());
    assert_eq!(cached.as_deref(), Some(SDP.as_bytes()), "the description alone");
}

#[test]
fn a_new_dialog_answer_caches_its_description_alone() {
    let call = call_with_early_a_dialog("A1early");
    let event = bobs_200(MULTIPART, &framed());
    let body =
        Body::new(framed().into_bytes(), Some(MULTIPART.into()), BodyAuthor::Leg("b-1".into()));
    let result = on_b(
        &call,
        &event,
        &[RuleAction::AnswerALegNewDialog {
            status: 200,
            reason: "OK".into(),
            body: Some(body),
            to_tag: None,
            header_updates: vec![],
            relayed: RelayedFinal::none(),
        }],
    );
    let d = result.call.a_leg.dialogs.confirmed().expect("a confirmed a-dialog");
    assert_eq!(d.cached_sdp.as_deref(), Some(SDP.as_bytes()), "the description alone");
}

/// The stack's re-INVITE and its in-dialog INVITE, each carrying only an
/// `early-session` description (RFC 3959), make no offer: the via-extension
/// says so, as the unheld path reads it.
#[test]
fn the_stacks_in_dialog_invite_offer_flag_reads_its_description() {
    let key = SigningKey::new(&[3u8; 32]).expect("a key");
    let config = B2buaConfig { cluster_key: Some(key.clone()), ..B2buaConfig::default() };
    let call = call_with_retained_answered_2xx();
    let event = CallEvent::sip(SipMessage::Request(invite()), "127.0.0.1:5060".parse().unwrap());
    let early = |disposition: &str| {
        let mut body = Body::own(SDP.as_bytes().to_vec(), Some("application/sdp".into()));
        body.descriptors = vec![sip_message::SipHeader {
            name: SipStr::from_static("Content-Disposition"),
            value: SipStr::owned(disposition),
        }];
        body
    };
    for disposition in ["early-session", "session"] {
        for action in [
            RuleAction::SendReinvite {
                leg_id: "a".into(),
                body: Some(early(disposition)),
                add_headers: vec![],
            },
            RuleAction::SendRequestToLeg {
                leg_id: "a".into(),
                method: "INVITE".into(),
                body: Some(early(disposition)),
                headers: vec![],
            },
        ] {
            let mut id_gen = scope(1);
            let result = exec_on(&call, &event, "b-1", &config, &mut id_gen, &[action]);
            let invite = sent(&result, Method::Invite).pop().expect("an INVITE leaves");
            let ext = b2bua_core::stack_identity::read_via_extension(
                invite.top_via(),
                "INVITE",
                Some(&key),
            )
            .expect("a verified extension");
            assert_eq!(ext.offer, disposition == "session", "{disposition}");
        }
    }
}

fn answering(answer: Option<&str>) -> RuleAction {
    RuleAction::AckLegAnswering {
        leg_id: "b-1".into(),
        answer: answer.map(|a| Body::from_leg(a.as_bytes().to_vec(), "a")),
    }
}

/// Bob's 200 to an INVITE that made no offer makes one: his ACK carries the
/// answer given, and waits for one when none is given yet.
#[test]
fn an_offering_2xx_takes_the_answer_or_waits_for_it() {
    let delayed = invite().thaw().without_body().freeze().expect("a delayed offer");
    let call = dialled_with(&delayed);
    let event = bobs_200("application/sdp", SDP);
    let confirm = RuleAction::ConfirmDialog { leg_id: "b-1".into() };
    let result = on_b(&call, &event, &[confirm.clone(), answering(Some(ANSWER))]);
    let ack = sent(&result, Method::Ack).pop().expect("an ACK");
    assert!(ack.sdp().is_some_and(|sdp| sdp.windows(5).any(|w| w == b"10000")), "the answer");
    let result = on_b(&call, &event, &[confirm, answering(None)]);
    assert!(sent(&result, Method::Ack).is_empty(), "the ACK waits for the answer");
}

/// Bob's 200 to an INVITE that made the offer makes none: his ACK is bare,
/// whatever answer is given.
#[test]
fn a_2xx_answering_the_invites_offer_draws_a_bare_ack() {
    let call = dialled_with(&invite());
    let event = bobs_200("application/sdp", SDP);
    let confirm = RuleAction::ConfirmDialog { leg_id: "b-1".into() };
    for answer in [Some(ANSWER), None] {
        let result = on_b(&call, &event, &[confirm.clone(), answering(answer)]);
        let ack = sent(&result, Method::Ack).pop().expect("an ACK");
        assert!(ack.body().is_empty(), "a bare ACK for {answer:?}");
    }
}

/// Bob's reliable 183 carried the offer his INVITE left out, absorbed by this
/// stack rather than shown to the caller, and its PRACK answered it (RFC 3262
/// §5): his 200 makes no offer, so his ACK is bare whatever answer is given.
#[test]
fn an_absorbed_reliable_offer_leaves_the_2xx_no_offer() {
    let delayed = invite().thaw().without_body().freeze().expect("a delayed offer");
    let mut call = dialled_with(&delayed);
    let id = call::ProvisionalId {
        leg: "b-1".into(),
        responder_tag: "bobtag".into(),
        invite_cseq: call::CSeq(1),
        rseq: call::RSeq(1),
    };
    call::helpers::record_provisional(&mut call, id, true);
    let event = bobs_200("application/sdp", SDP);
    let confirm = RuleAction::ConfirmDialog { leg_id: "b-1".into() };
    let result = on_b(&call, &event, &[confirm, answering(Some(ANSWER))]);
    let ack = sent(&result, Method::Ack).pop().expect("an ACK");
    assert!(ack.body().is_empty(), "the reliable 183 carried the offer: a bare ACK");
}
