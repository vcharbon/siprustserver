//! A method is case-sensitive (RFC 3261 §7.1): `invite` is an extension
//! method, never INVITE, and keeps its spelling on the wire; the CSeq names
//! the request line's method in the same case (§8.1.1.5).

use sip_message::{serialize, CustomParser, Method, SipMessage, SipParser};

fn request(method: &str, cseq_method: &str) -> Vec<u8> {
    format!(
        "{method} sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-case\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: case-1@192.0.2.10\r\n\
         CSeq: 1 {cseq_method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[test]
fn a_lower_case_invite_is_an_extension_method() {
    let msg = match CustomParser::new().parse(&request("invite", "invite")) {
        Ok(SipMessage::Request(r)) => r,
        other => panic!("an extension method parses: {other:?}"),
    };
    assert_eq!(*msg.method(), Method::Other("invite".to_string()));
    assert_ne!(*msg.method(), Method::Invite);
    assert_eq!(msg.cseq().method(), msg.method());
    let wire = serialize(&SipMessage::Request(msg));
    assert!(wire.starts_with(b"invite sip:bob@example.com SIP/2.0\r\n"), "rendered as written");
}

#[test]
fn the_cseq_names_the_method_in_its_case() {
    for (method, cseq) in [("INVITE", "invite"), ("invite", "INVITE"), ("Bye", "BYE")] {
        assert!(
            CustomParser::new().parse(&request(method, cseq)).is_err(),
            "{method} with CSeq {cseq} is a mismatch"
        );
    }
}

#[test]
fn a_method_compares_by_its_spelling() {
    assert_eq!(Method::from_wire("INVITE"), Method::Invite);
    assert_eq!(Method::from_wire("invite"), Method::Other("invite".to_string()));
    assert_eq!(Method::from_wire("Bye").as_str(), "Bye");
    assert!(Method::Invite == "INVITE");
    assert!(Method::Invite != "invite");
    assert!(Method::from_wire("invite") != "INVITE");
}
