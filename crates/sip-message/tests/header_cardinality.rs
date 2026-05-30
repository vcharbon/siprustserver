//! Header-cardinality enforcement (RFC 3261 §8.1.1 / §8.1.1.8 / §12.1.1). Port
//! of `tests/sip/header-cardinality.test.ts`.
//!
//! From / To / Call-ID / CSeq appear exactly once; a duplicate is rejected.
//! Contact may repeat only where a list is legal (3xx redirects, REGISTER);
//! elsewhere a second Contact — or the REGISTER-only `*` wildcard — is rejected.
//! Absence of a Contact is NOT a parser error (a handler-level 400 concern).

use sip_message::{CustomParser, SipParser};

fn rejects(raw: &str) -> bool {
    CustomParser::new().parse(raw.as_bytes()).is_err()
}

fn accepts(raw: &str) -> bool {
    CustomParser::new().parse(raw.as_bytes()).is_ok()
}

const DUP_FROM_REQUEST: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-dupfrom\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
From: <sip:eve@example.test>;tag=e\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: dup-from-req\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const DUP_TO_REQUEST: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-dupto\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
To: <sip:carol@example.test>\r\n\
Call-ID: dup-to-req\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const DUP_CALL_ID_REQUEST: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-dupcid\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: dup-cid-1\r\n\
Call-ID: dup-cid-2\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const DUP_CSEQ_REQUEST: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-dupcseq\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: dup-cseq-req\r\n\
CSeq: 1 INVITE\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const DUP_FROM_RESPONSE: &str = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-dupfromresp\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
From: <sip:eve@example.test>;tag=e\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: dup-from-resp\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Content-Length: 0\r\n\r\n";

const MULTI_CONTACT_INVITE: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-multic\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: multi-contact-invite\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Contact: <sip:alice@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n";

const MULTI_CONTACT_OK: &str = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-multiok\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: multi-contact-ok\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>\r\n\
Contact: <sip:bob@10.0.0.3:5060>\r\n\
Content-Length: 0\r\n\r\n";

const WILDCARD_INVITE: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-wcinv\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: wildcard-invite\r\n\
CSeq: 1 INVITE\r\n\
Contact: *\r\n\
Content-Length: 0\r\n\r\n";

const REDIRECT_MULTI_CONTACT: &str = "SIP/2.0 302 Moved Temporarily\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-302list\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>;tag=b\r\n\
Call-ID: redirect-list\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5060>;q=0.8\r\n\
Contact: <sip:bob@10.0.0.3:5060>;q=0.5\r\n\
Content-Length: 0\r\n\r\n";

const REGISTER_MULTI_CONTACT: &str = "REGISTER sip:registrar.example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-reglist\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:alice@example.test>\r\n\
Call-ID: register-list\r\n\
CSeq: 1 REGISTER\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Contact: <sip:alice@10.0.0.2:5060>\r\n\
Content-Length: 0\r\n\r\n";

const CLEAN_INVITE: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-clean\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: clean-invite\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const CONTACTLESS_INVITE: &str = "INVITE sip:bob@example.test SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-nocontact\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.test>;tag=a\r\n\
To: <sip:bob@example.test>\r\n\
Call-ID: contactless-invite\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";

// --- exactly-once headers ---

#[test]
fn duplicate_from_to_callid_cseq_rejected() {
    assert!(rejects(DUP_FROM_REQUEST), "dup From");
    assert!(rejects(DUP_TO_REQUEST), "dup To");
    assert!(rejects(DUP_CALL_ID_REQUEST), "dup Call-ID");
    assert!(rejects(DUP_CSEQ_REQUEST), "dup CSeq");
    assert!(rejects(DUP_FROM_RESPONSE), "dup From (response)");
}

// --- Contact cardinality ---

#[test]
fn contact_cardinality() {
    assert!(rejects(MULTI_CONTACT_INVITE), "multiple Contact in INVITE");
    assert!(rejects(MULTI_CONTACT_OK), "multiple Contact in 200-to-INVITE");
    assert!(rejects(WILDCARD_INVITE), "Contact: * in non-REGISTER");
    assert!(accepts(REDIRECT_MULTI_CONTACT), "multiple Contact in 302 redirect");
    assert!(accepts(REGISTER_MULTI_CONTACT), "multiple Contact in REGISTER");
}

// --- regression ---

#[test]
fn clean_and_contactless_invites_parse() {
    assert!(accepts(CLEAN_INVITE), "clean single-everything INVITE");
    assert!(accepts(CONTACTLESS_INVITE), "Contact-less INVITE (handler concern)");
}
