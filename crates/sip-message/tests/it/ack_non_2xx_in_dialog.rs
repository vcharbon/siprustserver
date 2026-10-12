//! The ACK for a non-2xx final to an in-dialog INVITE, built from the dialog
//! and the response alone (RFC 3261 §17.1.1.3), is the ACK the INVITE itself
//! would give: same Request-URI, Route set, Via, From, To, Call-ID and CSeq.

use sip_message::generators::{
    generate_ack_for_non_2xx, generate_ack_for_non_2xx_in_dialog, generate_in_dialog_request,
    generate_response, GenerateInDialogRequestOpts, GenerateResponseOpts, InDialogMethod,
    StackDialog,
};
use sip_message::header::{self, HeaderName, Uri, Via};
use sip_message::{SipRequest, SipStr};

fn dialog(route_set: Vec<String>) -> StackDialog {
    StackDialog {
        call_id: "call-bleg-1".to_string(),
        local_tag: "b2bua-local".to_string(),
        remote_tag: "bob-remote".to_string(),
        local_uri: "sip:b2bua@10.0.0.1:5060".to_string(),
        remote_uri: "sip:bob@192.0.2.20:5060".to_string(),
        remote_target: "sip:bob@192.0.2.20:5060".to_string(),
        local_cseq: 7,
        route_set,
    }
}

/// The re-INVITE `d` sends, and the 488 its peer answers it with.
fn round(d: &StackDialog) -> (SipRequest, sip_message::SipResponse) {
    let contact = Uri::parse(&SipStr::owned("sip:b2bua@10.0.0.1:5060")).expect("a URI");
    let invite = generate_in_dialog_request(
        InDialogMethod::Invite,
        d,
        &GenerateInDialogRequestOpts {
            via: Some(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bKround8")),
            contact: Some(header::Contact::from_uri(contact)),
            ..Default::default()
        },
    )
    .request;
    let refused =
        generate_response(&invite, 488, "Not Acceptable Here", &GenerateResponseOpts::default());
    (invite, refused)
}

/// The header lines of `name` in `req`, in wire order.
fn lines(req: &SipRequest, name: HeaderName) -> Vec<String> {
    req.raw_text(name).map(|l| l.to_string()).collect()
}

fn assert_same_ack(d: &StackDialog) {
    let (invite, refused) = round(d);
    let from_invite = generate_ack_for_non_2xx(&invite, &refused, &[]);
    let from_dialog = generate_ack_for_non_2xx_in_dialog(d, &refused);
    assert_eq!(from_dialog.request_uri().text(), from_invite.request_uri().text());
    for name in [
        HeaderName::Via,
        HeaderName::Route,
        HeaderName::From,
        HeaderName::To,
        HeaderName::CallId,
        HeaderName::CSeq,
    ] {
        assert_eq!(
            lines(&from_dialog, name.clone()),
            lines(&from_invite, name.clone()),
            "{name:?}"
        );
    }
    assert!(from_dialog.body().is_empty(), "the ACK carries no body");
}

#[test]
fn the_ack_built_from_the_dialog_is_the_invites_own() {
    assert_same_ack(&dialog(vec![]));
}

#[test]
fn the_ack_built_from_the_dialog_follows_the_invites_loose_route_set() {
    assert_same_ack(&dialog(vec![
        "<sip:proxy1.example.com;lr>".to_string(),
        "<sip:proxy2.example.com;lr>".to_string(),
    ]));
}

#[test]
fn the_ack_built_from_the_dialog_follows_the_invites_strict_route_set() {
    // A first route without `lr`: the INVITE's Request-URI is that route and
    // the remote target rides last (RFC 3261 §12.2.1.1); its ACK the same.
    assert_same_ack(&dialog(vec![
        "<sip:strict1.example.com>".to_string(),
        "<sip:proxy2.example.com;lr>".to_string(),
    ]));
}
