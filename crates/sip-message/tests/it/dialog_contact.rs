//! The Contact of a message that can establish a dialog is a SIP or SIPS URI
//! (RFC 3261 §8.1.1.8 for the request, §12.1.1 for the 1xx / 2xx answering
//! it): another scheme refuses such a message, and is read anywhere else.

use sip_message::{CustomParser, SipMessage, SipParser};

const NON_SIP: [&str; 2] = ["<urn:ietf:params:x:y>", "<http://example.com/a>"];

fn parse(raw: &str) -> Result<SipMessage, sip_message::SipParseError> {
    CustomParser::new().parse(raw.as_bytes())
}

/// A request of `method` (CSeq 1) carrying `contact` and `extra` lines.
fn request(method: &str, request_uri: &str, contact: &str, extra: &str) -> String {
    format!(
        "{method} {request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-dc1\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>\r\n\
         Call-ID: dialog-contact-1@192.0.2.1\r\nCSeq: 1 {method}\r\n\
         Contact: {contact}\r\n{extra}Content-Length: 0\r\n\r\n"
    )
}

/// A `status` response to a `method` request, carrying `contact`.
fn response(status: &str, method: &str, contact: &str) -> String {
    format!(
        "SIP/2.0 {status}\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-dc1\r\n\
         From: <sip:alice@example.com>;tag=a1\r\nTo: <sip:bob@example.com>;tag=b1\r\n\
         Call-ID: dialog-contact-1@192.0.2.1\r\nCSeq: 1 {method}\r\n\
         Contact: {contact}\r\nContent-Length: 0\r\n\r\n"
    )
}

#[test]
fn an_invite_naming_a_non_sip_contact_is_refused_with_a_partial() {
    for contact in NON_SIP {
        let err = parse(&request("INVITE", "sip:bob@example.com", contact, "")).expect_err(contact);
        assert!(err.reason.contains("RFC 3261 §8.1.1.8"), "{contact}: {}", err.reason);
        assert!(err.partial.is_some(), "{contact}: a 400 answers the INVITE");
    }
}

#[test]
fn a_subscribe_naming_a_non_sip_contact_is_refused() {
    for contact in NON_SIP {
        let raw = request("SUBSCRIBE", "sip:bob@example.com", contact, "Event: presence\r\n");
        let err = parse(&raw).expect_err(contact);
        assert!(err.reason.contains("§12.1.1"), "{contact}: {}", err.reason);
    }
}

#[test]
fn a_dialog_establishing_response_naming_a_non_sip_contact_is_refused() {
    for (status, method) in
        [("200 OK", "INVITE"), ("180 Ringing", "INVITE"), ("200 OK", "SUBSCRIBE")]
    {
        for contact in NON_SIP {
            let err = parse(&response(status, method, contact)).expect_err(contact);
            assert!(err.reason.contains("§12.1.1"), "{status} {method} {contact}: {}", err.reason);
        }
    }
}

/// A REGISTER binding, a 3xx alternative and an OPTIONS answer name no dialog
/// target: a Contact of any scheme reads there.
#[test]
fn a_non_sip_contact_outside_a_dialog_is_read() {
    for contact in NON_SIP {
        let register = request("REGISTER", "sip:example.com", contact, "Expires: 3600\r\n");
        parse(&register).unwrap_or_else(|e| panic!("REGISTER {contact}: {}", e.reason));
        let options = request("OPTIONS", "sip:bob@example.com", contact, "");
        parse(&options).unwrap_or_else(|e| panic!("OPTIONS {contact}: {}", e.reason));
        for (status, method) in [("302 Moved Temporarily", "INVITE"), ("200 OK", "OPTIONS")] {
            parse(&response(status, method, contact))
                .unwrap_or_else(|e| panic!("{status} {method} {contact}: {}", e.reason));
        }
    }
}

#[test]
fn a_sip_or_sips_contact_establishes_the_dialog() {
    for contact in ["<sip:alice@192.0.2.1:5060>", "<sips:alice@example.com>"] {
        parse(&request("INVITE", "sip:bob@example.com", contact, ""))
            .unwrap_or_else(|e| panic!("INVITE {contact}: {}", e.reason));
        parse(&response("200 OK", "INVITE", contact))
            .unwrap_or_else(|e| panic!("200 {contact}: {}", e.reason));
    }
}
