//! The generators that build a message out of a received one, run on a parsed
//! input: the ACK and BYE a final draws from the INVITE it answers, the
//! in-dialog requests of the dialog a response opens, and the response a
//! relay returns to a request's sender from the lines that request stated.
//!
//! On a strictly read input none of them may refuse to freeze: every line it
//! echoes is one the strict reader took, so an `Unfreezable` there is a line
//! the read admits and the build rejects, and the lane fails on it.

use sip_message::draft::Entry;
use sip_message::generators::{
    generate_ack_for_2xx, generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx,
    generate_ack_for_non_2xx_in_dialog, generate_bye_for_2xx_from_invite,
    generate_in_dialog_request, generate_relayed_response, GenerateAckFor2xxOpts, GenerateError,
    GenerateInDialogRequestOpts, GenerateRelayedResponseOpts, InDialogMethod, StackDialog,
};
use sip_message::header::{HeaderName, HeaderValue, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{InDialogRequest, SipMessage, SipParser, SipRequest, SipResponse, SipStr};

use super::probe::Digest;

/// The INVITE this side sent, which every response is read as answering.
const INVITE: &str = "INVITE sip:b@192.0.2.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-echo\r\n\
From: <sip:a@192.0.2.1>;tag=a1\r\nTo: <sip:b@192.0.2.2>\r\nCall-ID: echo@192.0.2.1\r\n\
CSeq: 1 INVITE\r\nContact: <sip:a@192.0.2.1:5060>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";

/// The remote target of the dialog an earlier 2xx opened.
const TARGET: &str = "sip:b@192.0.2.2:5070";

/// The in-dialog methods a dialog carries.
const METHODS: &[InDialogMethod] = &[
    InDialogMethod::Bye,
    InDialogMethod::Invite,
    InDialogMethod::Prack,
    InDialogMethod::Info,
    InDialogMethod::Update,
    InDialogMethod::Options,
];

fn invite() -> SipRequest {
    match CustomParser::new().parse(INVITE.as_bytes()) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("the lane's INVITE reads: {other:?}"),
    }
}

/// Fold the message `built` in under `label`; on a strictly read input a
/// message that does not freeze fails the lane.
pub(super) fn outcome(
    label: &str,
    built: Result<SipRequest, GenerateError>,
    strict: bool,
    d: &mut Digest,
) {
    outcome_image(label, built.map(|m| m.image().to_vec()), strict, d);
}

pub(super) fn outcome_image(
    label: &str,
    built: Result<Vec<u8>, GenerateError>,
    strict: bool,
    d: &mut Digest,
) {
    match built {
        Ok(image) => d.raw(label, &image),
        Err(GenerateError::Unfreezable(e)) if strict => {
            panic!("{label}: a strictly read input built a message that does not freeze: {e}")
        }
        Err(e) => d.note(label, &e.to_string()),
    }
}

/// The dialog `resp` belongs to for the side that sent [`INVITE`], this
/// side's own addresses and the peer's tag. A response that opens one (a
/// tagged 101-299, RFC 3261 §12.1.2) gives it its Contact and recorded routes;
/// a later final belongs to the dialog an earlier 2xx opened, at [`TARGET`].
fn dialog_of(invite: &SipRequest, resp: &SipResponse) -> StackDialog {
    let opens = (101..300).contains(&resp.status());
    let contact = resp.contacts().as_slice().first().map(|c| c.uri().text().into_owned());
    let routes = resp
        .record_route_set()
        .map(|set| set.reversed().iter().map(HeaderValue::to_wire).collect())
        .unwrap_or_default();
    StackDialog {
        call_id: invite.call_id().to_string(),
        local_tag: invite.from().tag().unwrap_or_default().to_string(),
        remote_tag: resp.to().tag().unwrap_or_default().to_string(),
        local_uri: invite.from().uri().text().into_owned(),
        remote_uri: invite.to().uri().text().into_owned(),
        remote_target: if opens { contact.unwrap_or_default() } else { TARGET.into() },
        local_cseq: 1,
        route_set: if opens { routes } else { Vec::new() },
    }
}

/// Every message a UA builds out of `resp`, read as answering [`INVITE`].
pub fn from_response(resp: &SipResponse, strict: bool, d: &mut Digest) {
    let invite = invite();
    outcome("ack-non-2xx", generate_ack_for_non_2xx(&invite, resp, &[]), strict, d);
    if resp.to().tag().is_none() {
        return;
    }
    if (200..300).contains(&resp.status()) {
        let ack = generate_ack_for_2xx_from_invite(&invite, resp, "z9hG4bK-echo-ack");
        outcome("ack-2xx", ack, strict, d);
        let bye = generate_bye_for_2xx_from_invite(&invite, resp, 2, "z9hG4bK-echo-bye");
        outcome("bye-2xx", bye, strict, d);
    }
    let dialog = dialog_of(&invite, resp);
    if dialog.remote_target.is_empty() {
        return;
    }
    let ack = generate_ack_for_non_2xx_in_dialog(&dialog, resp);
    outcome("ack-non-2xx-in-dialog", ack, strict, d);
    let via = || Some(Via::udp("192.0.2.1", 5060).with_branch(SipStr::from_static("z9hG4bK-e")));
    let opts = GenerateAckFor2xxOpts { via: via(), ..Default::default() };
    outcome("ack-in-dialog", generate_ack_for_2xx(None, &dialog, &opts), strict, d);
    let opts = GenerateInDialogRequestOpts { via: via(), ..Default::default() };
    for &method in METHODS {
        let built = generate_in_dialog_request(method, &dialog, &opts).map(|r| r.request);
        outcome("in-dialog", built, strict, d);
    }
}

/// The response a relay returns to `req`'s sender, built from the lines `req`
/// stated, as a back-to-back UA's snapshot keeps them.
pub fn from_request(req: &SipRequest, strict: bool, d: &mut Digest) {
    if InDialogRequest::new(req).is_err() {
        return;
    }
    let echo = |name: HeaderName| -> Vec<Entry> {
        req.raw_text(name.clone()).map(|line| Entry::raw(name.clone(), line)).collect()
    };
    let first = |name: HeaderName| echo(name).into_iter().next();
    let opts = GenerateRelayedResponseOpts {
        vias: echo(HeaderName::Via),
        from: first(HeaderName::From),
        to: first(HeaderName::To),
        call_id: first(HeaderName::CallId),
        cseq: first(HeaderName::CSeq),
        ..Default::default()
    };
    for (status, reason) in [(200, "OK"), (481, "Call/Transaction Does Not Exist")] {
        let built = generate_relayed_response(status, reason, &opts).map(|m| m.image().to_vec());
        outcome_image("relayed", built, strict, d);
    }
}
