//! Contact-set extraction — the eager, validate-all Contact model. Port of
//! `tests/sip/contact-set.test.ts`.
//!
//! `.contacts` returns every contact (folded across repeated header lines and
//! comma-separated values, RFC 3261 §7.3.1) or the `*` wildcard (§10.2.2).
//! Every contact URI is validated at parse time, so a malformed entry anywhere
//! rejects the whole message.

use sip_message::{Contact, ContactSet, CustomParser, SipMessage, SipParser};

fn parse_ok(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).expect("parse should succeed")
}

fn parse_fails(raw: &str) -> bool {
    CustomParser::new().parse(raw.as_bytes()).is_err()
}

fn contacts_of(msg: &SipMessage) -> &ContactSet {
    match msg {
        SipMessage::Request(r) => &r.contacts,
        SipMessage::Response(r) => &r.contacts,
    }
}

fn first_contact_uri(msg: &SipMessage) -> Option<&str> {
    match contacts_of(msg) {
        ContactSet::Wildcard => None,
        ContactSet::Contacts(cs) => cs.first().map(|c| c.uri.as_str()),
    }
}

fn uris(cs: &[Contact]) -> Vec<&str> {
    cs.iter().map(|c| c.uri.as_str()).collect()
}

const SINGLE_INVITE: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-single\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: contacts-single\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const REDIRECT_MULTI_LINE: &str = "SIP/2.0 302 Moved Temporarily\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-multi\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: contacts-multi\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>;q=0.8\r\n\
Contact: <sip:bob@10.0.0.3:5060>;q=0.5\r\n\
Content-Length: 0\r\n\r\n";

const REDIRECT_COMMA_LIST: &str = "SIP/2.0 302 Moved Temporarily\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-comma\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: contacts-comma\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>;q=0.8, <sip:bob@10.0.0.3:5060>;q=0.5\r\n\
Content-Length: 0\r\n\r\n";

const REGISTER_WILDCARD: &str = "REGISTER sip:registrar.example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-star\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:alice@example.test>\r\n\
Call-ID: contacts-star\r\n\
CSeq: 1 REGISTER\r\n\
Contact: *\r\n\
Expires: 0\r\n\
Content-Length: 0\r\n\r\n";

const REDIRECT_BAD_SECOND_CONTACT: &str = "SIP/2.0 302 Moved Temporarily\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bad\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: contacts-bad\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Contact: <sip:@>\r\n\
Content-Length: 0\r\n\r\n";

const REGISTER_WILDCARD_MIXED: &str = "REGISTER sip:registrar.example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-mixed\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:alice@example.test>\r\n\
Call-ID: contacts-mixed\r\n\
CSeq: 1 REGISTER\r\n\
Contact: *\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Expires: 0\r\n\
Content-Length: 0\r\n\r\n";

#[test]
fn single_contact_list_holds_one() {
    let msg = parse_ok(SINGLE_INVITE);
    let ContactSet::Contacts(cs) = contacts_of(&msg) else { panic!("non-wildcard") };
    assert_eq!(uris(cs), vec!["sip:alice@10.0.0.1:5060"]);
    assert_eq!(first_contact_uri(&msg), Some("sip:alice@10.0.0.1:5060"));
}

#[test]
fn multiple_contact_lines_fold_with_params() {
    let msg = parse_ok(REDIRECT_MULTI_LINE);
    let ContactSet::Contacts(cs) = contacts_of(&msg) else { panic!("non-wildcard") };
    assert_eq!(uris(cs), vec!["sip:bob@10.0.0.2:5060", "sip:bob@10.0.0.3:5060"]);
    assert_eq!(param(&cs[0], "q"), Some("0.8"));
    assert_eq!(param(&cs[1], "q"), Some("0.5"));
    assert_eq!(first_contact_uri(&msg), Some("sip:bob@10.0.0.2:5060"));
}

#[test]
fn comma_separated_contacts_fold_identically() {
    let msg = parse_ok(REDIRECT_COMMA_LIST);
    let ContactSet::Contacts(cs) = contacts_of(&msg) else { panic!("non-wildcard") };
    assert_eq!(uris(cs), vec!["sip:bob@10.0.0.2:5060", "sip:bob@10.0.0.3:5060"]);
}

#[test]
fn wildcard_contact_has_no_single_accessor() {
    let msg = parse_ok(REGISTER_WILDCARD);
    assert!(matches!(contacts_of(&msg), ContactSet::Wildcard));
    assert_eq!(first_contact_uri(&msg), None);
}

#[test]
fn malformed_contact_anywhere_rejects_message() {
    assert!(parse_fails(REDIRECT_BAD_SECOND_CONTACT));
}

#[test]
fn wildcard_mixed_with_real_contact_is_rejected() {
    assert!(parse_fails(REGISTER_WILDCARD_MIXED));
}

fn param<'a>(c: &'a Contact, key: &str) -> Option<&'a str> {
    match c.params.get(key) {
        Some(sip_message::ParamValue::Value(v)) => Some(v.as_str()),
        _ => None,
    }
}
