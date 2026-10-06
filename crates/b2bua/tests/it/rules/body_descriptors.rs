//! A body a rule action takes whole from a received message keeps the lines
//! that message stated to describe it (RFC 3261 §20.11 / §20.12 / §20.15,
//! RFC 2045): every emitter states them beside the body, and a body keeps the
//! media type it was given.

use super::*;
use b2bua::effects::OutboundBody;
use call::{HostPort, InviteTxnHandle};

const SDP: &str = "v=0\r\no=b 1 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";

/// The lines of the message the body was taken from: its role and framing,
/// beside lines that describe no body.
fn source_lines() -> Vec<sip_message::SipHeader> {
    [
        ("Content-Disposition", "session;handling=required"),
        ("MIME-Version", "1.0"),
        ("P-Vendor", "x"),
        ("Content-Type", "application/sdp"),
    ]
    .into_iter()
    .map(|(n, v)| sip_message::SipHeader {
        name: SipStr::from_static(n),
        value: SipStr::from_static(v),
    })
    .collect()
}

fn described() -> Body {
    Body::from_leg(SDP.as_bytes().to_vec(), "b-1").described_by(&source_lines())
}

/// Alice ↔ b-1, both dialogs confirmed.
fn bridged() -> call::Call {
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.dialogs = vec![Dialog {
        sip: StackDialog {
            call_id: call.a_leg.call_id.clone(),
            local_tag: "a-svc".into(),
            remote_tag: call.a_leg.from_tag.clone(),
            local_uri: "sip:svc@10.0.0.9".into(),
            remote_uri: "sip:alice@host".into(),
            remote_target: "sip:alice@127.0.0.1:5060".into(),
            local_cseq: 1,
            route_set: vec![],
        },
        ext: b_leg_pending().dialogs[0].ext.clone(),
    }];
    let mut leg = b_leg_pending();
    leg.state = LegState::Confirmed;
    leg.dialogs[0].sip.remote_tag = "b-tag".into();
    leg.dialogs[0].ext.pending_invite_txn = Some(InviteTxnHandle {
        branch: "z9hG4bKb1".into(),
        original_invite: invite().image().to_vec(),
        destination: HostPort { host: "10.0.0.2".into(), port: 5070 },
    });
    call.b_legs = vec![leg];
    call
}

fn sent_request(result: &HandlerResult, leg: &str) -> SipRequest {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.clone()),
            _ => None,
        })
        .expect("a request on the leg")
}

fn values(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The lines describing the body ride, those describing nothing do not, and
/// the body's own media type is stated once.
fn assert_described(req: &SipRequest) {
    assert_eq!(values(req, "Content-Disposition"), ["session;handling=required"]);
    assert_eq!(values(req, "MIME-Version"), ["1.0"]);
    assert!(values(req, "P-Vendor").is_empty(), "a line describing no body stays behind");
    assert_eq!(values(req, "Content-Type"), ["application/sdp"]);
}

#[test]
fn a_reinvite_states_the_lines_describing_the_body_it_carries() {
    let action = RuleAction::SendReinvite {
        leg_id: "a".into(),
        body: Some(described()),
        add_headers: vec![],
    };
    assert_described(&sent_request(&execute_on(&bridged(), &[action]), "a"));
}

#[test]
fn an_ack_states_the_lines_describing_the_answer_it_carries() {
    let action = RuleAction::AckLeg { leg_id: "b-1".into(), body: Some(described()) };
    assert_described(&sent_request(&execute_on(&bridged(), &[action]), "b-1"));
}

#[test]
fn an_in_dialog_request_states_the_lines_describing_its_body() {
    let action = RuleAction::SendRequestToLeg {
        leg_id: "b-1".into(),
        method: "INFO".into(),
        body: Some(described()),
        headers: vec![],
    };
    assert_described(&sent_request(&execute_on(&bridged(), &[action]), "b-1"));
}

/// A line the action states itself is the action's: the body's own does not
/// compete with it.
#[test]
fn a_stated_line_outranks_the_bodys_own() {
    let action = RuleAction::SendRequestToLeg {
        leg_id: "b-1".into(),
        method: "INFO".into(),
        body: Some(described()),
        headers: vec![("Content-Disposition".into(), "render".into())],
    };
    let req = sent_request(&execute_on(&bridged(), &[action]), "b-1");
    assert_eq!(values(&req, "Content-Disposition"), ["render"]);
}

/// A re-INVITE carries its body under the media type the body was given.
#[test]
fn a_reinvite_keeps_the_bodys_own_media_type() {
    let body = Body::new(b"<x/>".to_vec(), Some("application/xml".into()), BodyAuthor::Stack);
    let action =
        RuleAction::SendReinvite { leg_id: "b-1".into(), body: Some(body), add_headers: vec![] };
    let req = sent_request(&execute_on(&bridged(), &[action]), "b-1");
    assert_eq!(values(&req, "Content-Type"), ["application/xml"]);
}

/// A message sending no body states nothing about one.
#[test]
fn no_body_no_description() {
    let empty = Body::from_leg(Vec::new(), "b-1").described_by(&source_lines());
    let action =
        RuleAction::SendReinvite { leg_id: "a".into(), body: Some(empty), add_headers: vec![] };
    let req = sent_request(&execute_on(&bridged(), &[action]), "a");
    for name in ["Content-Disposition", "MIME-Version", "Content-Type"] {
        assert!(values(&req, name).is_empty(), "no {name}");
    }
}
