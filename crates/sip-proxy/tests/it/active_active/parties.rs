//! What the parties of a cell send: the caller's and a worker's dialling
//! INVITE, the CANCEL of one (RFC 3261 §9.1), a UAS's response to a request
//! it received (§8.2.6), the ACK of a final (§17.1.1.3, §13.2.2.4), and an
//! in-dialog request on the route set a 2xx fixed (§12.2.1.1).

use std::net::SocketAddr;

use sip_message::generators::{
    generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx, generate_response,
    GenerateResponseOpts,
};
use sip_message::header::{Contact, HeaderValue};
use sip_message::parser::custom::CustomParser;
use sip_message::{HeaderName, SipMessage, SipParser, SipRequest, SipResponse, SipStr};

use super::rig::Party;

fn parse_request(raw: &str) -> SipRequest {
    match CustomParser::default().parse(raw.as_bytes()) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("a request: {other:?}"),
    }
}

/// The branch a party opens a transaction with: the magic cookie and `token`.
pub fn branch(token: &str) -> String {
    format!("z9hG4bK-{}", word(token))
}

/// `text` as a SIP token: `@` and `.` turned to `-`.
fn word(text: &str) -> String {
    text.replace(['@', '.'], "-")
}

/// The caller's dialling INVITE of call `call_id`.
pub fn caller_invite(call_id: &str) -> SipRequest {
    let at = Party::Caller.addr();
    let callee = Party::Callee.addr();
    parse_request(&format!(
        "INVITE sip:bob@{callee} SIP/2.0\r\n\
Via: SIP/2.0/UDP {at};branch={};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{at}>;tag=caller-{}\r\n\
To: <sip:bob@{callee}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{at}>\r\n\
Content-Length: 0\r\n\r\n",
        branch(call_id),
        word(call_id)
    ))
}

/// A worker's dialling INVITE of its leg `call_id` toward the callee.
pub fn worker_invite(worker: Party, call_id: &str) -> SipRequest {
    let at = worker.addr();
    let callee = Party::Callee.addr();
    parse_request(&format!(
        "INVITE sip:bob@{callee} SIP/2.0\r\n\
Via: SIP/2.0/UDP {at};branch={};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{at}>;tag=leg-{}\r\n\
To: <sip:bob@{callee}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{at}>\r\n\
Content-Length: 0\r\n\r\n",
        branch(call_id),
        word(call_id)
    ))
}

/// The CANCEL of `invite` (§9.1): its Request-URI, top Via, From, To,
/// Call-ID, CSeq number and Route.
pub fn cancel_of(invite: &SipRequest) -> SipRequest {
    let line = |name: HeaderName| invite.raw_text(name).next().expect("echoed").to_string();
    let routes: String =
        invite.raw_text(HeaderName::Route).map(|r| format!("Route: {r}\r\n")).collect();
    parse_request(&format!(
        "CANCEL {} SIP/2.0\r\n\
Via: {}\r\n\
Max-Forwards: 70\r\n\
From: {}\r\n\
To: {}\r\n\
Call-ID: {}\r\n\
CSeq: {} CANCEL\r\n\
{routes}Content-Length: 0\r\n\r\n",
        invite.request_uri(),
        line(HeaderName::Via),
        line(HeaderName::From),
        line(HeaderName::To),
        line(HeaderName::CallId),
        invite.cseq().seq(),
    ))
}

/// The response of the UAS at `at` to `req`, received from the instance node
/// `from`: `status` with `to_tag`, a dialog-creating one with `at` as Contact.
pub fn answer(
    req: &SipRequest,
    from: SocketAddr,
    at: Party,
    status: u16,
    reason: &str,
    to_tag: &str,
) -> SipResponse {
    let contact = (status < 300 && status > 100).then(|| {
        let uri = format!("sip:uas@{}", at.addr());
        Contact::parse(&SipStr::owned(&format!("<{uri}>"))).expect("a contact")
    });
    let opts = GenerateResponseOpts {
        to_tag: Some(to_tag.to_string()),
        contact,
        incoming_source: Some((from.ip().to_string(), from.port())),
        ..Default::default()
    };
    generate_response(req, status, reason, &opts).expect("a response")
}

/// The ACK of the non-2xx `final_response` to `invite` (§17.1.1.3).
pub fn ack_non_2xx(invite: &SipRequest, final_response: &SipResponse) -> SipRequest {
    generate_ack_for_non_2xx(invite, final_response, &[]).expect("an ACK")
}

/// The ACK of the 2xx `final_response` to `invite` (§13.2.2.4): its own
/// transaction, on the route set the 2xx fixed.
pub fn ack_2xx(invite: &SipRequest, final_response: &SipResponse) -> SipRequest {
    let token = format!("ack-{}", invite.call_id().as_str());
    generate_ack_for_2xx_from_invite(invite, final_response, &branch(&token), None).expect("an ACK")
}

/// An in-dialog `method` request, CSeq `cseq`, from the side whose 2xx ACK
/// was `ack`: the same Request-URI, route set, From, To and Call-ID, on a
/// fresh transaction.
pub fn in_dialog(ack: &SipRequest, method: &str, cseq: u32) -> SipRequest {
    let line = |name: HeaderName| ack.raw_text(name).next().expect("echoed").to_string();
    let routes: String =
        ack.raw_text(HeaderName::Route).map(|r| format!("Route: {r}\r\n")).collect();
    let via = ack.top_via();
    let (host, port) = via.sent_by().pair();
    let token = format!("{method}-{cseq}-{}", ack.call_id().as_str());
    parse_request(&format!(
        "{method} {} SIP/2.0\r\n\
Via: SIP/2.0/UDP {host}:{port};branch={};rport\r\n\
Max-Forwards: 70\r\n\
From: {}\r\n\
To: {}\r\n\
Call-ID: {}\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:alice@{host}:{port}>\r\n\
{routes}Content-Length: 0\r\n\r\n",
        ack.request_uri(),
        branch(&token),
        line(HeaderName::From),
        line(HeaderName::To),
        line(HeaderName::CallId),
    ))
}

/// The top Via branch of `req`.
pub fn top_branch(req: &SipRequest) -> String {
    req.top_via().branch().expect("a branch").to_string()
}

/// An in-dialog `method` request, CSeq `cseq`, from the UAS `uas` that
/// received `invite` and answered it with `to_tag` (§12.1.1): the route set
/// is the INVITE's Record-Route in order, the target its Contact.
pub fn uas_in_dialog(
    invite: &SipRequest,
    uas: Party,
    to_tag: &str,
    method: &str,
    cseq: u32,
) -> SipRequest {
    let line = |name: HeaderName| invite.raw_text(name).next().expect("echoed").to_string();
    let routes: String =
        invite.raw_text(HeaderName::RecordRoute).map(|r| format!("Route: {r}\r\n")).collect();
    let target = invite.contacts().as_slice().first().expect("a Contact").uri().to_string();
    let at = uas.addr();
    let token = format!("{method}-{cseq}-uas-{}", invite.call_id().as_str());
    parse_request(&format!(
        "{method} {target} SIP/2.0\r\n\
Via: SIP/2.0/UDP {at};branch={};rport\r\n\
Max-Forwards: 70\r\n\
From: {};tag={to_tag}\r\n\
To: {}\r\n\
Call-ID: {}\r\n\
CSeq: {cseq} {method}\r\n\
Contact: <sip:uas@{at}>\r\n\
{routes}Content-Length: 0\r\n\r\n",
        branch(&token),
        line(HeaderName::To),
        line(HeaderName::From),
        line(HeaderName::CallId),
    ))
}
