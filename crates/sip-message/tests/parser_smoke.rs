//! Walking-skeleton smoke tests for the ported custom parser. Full RFC 4475
//! torture + compliance matrix port is tracked separately (MIGRATION_STATUS).

use sip_message::{CustomParser, InDialogRequest, SipMessage, SipParser, SipResponseTagged};

fn parse(raw: &[u8]) -> Result<SipMessage, sip_message::SipParseError> {
    CustomParser::new().parse(raw)
}

const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:bob@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

const BYE: &[u8] = b"BYE sip:alice@pc33.example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK2\r\n\
From: Bob <sip:bob@example.com>;tag=as83kf\r\n\
To: Alice <sip:alice@example.com>;tag=1928\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 1 BYE\r\n\
Content-Length: 0\r\n\r\n";

#[test]
fn parses_well_formed_invite() {
    let msg = parse(INVITE).expect("INVITE should parse");
    let SipMessage::Request(req) = msg else { panic!("expected request") };
    assert_eq!(req.method, "INVITE");
    assert_eq!(req.uri, "sip:bob@example.com");
    assert_eq!(req.from.uri, "sip:alice@example.com");
    assert_eq!(req.from.tag.as_deref(), Some("1928"));
    assert_eq!(req.to.tag, None); // initial INVITE: no To-tag yet
    assert_eq!(req.call_id, "a84b4c76e66710@pc33.example.com");
    assert_eq!(req.cseq.seq, 314159);
    assert_eq!(req.cseq.method, "INVITE");
    assert_eq!(req.via.first().branch.as_deref(), Some("z9hG4bK1"));
    assert_eq!(req.via.first().host, "host.example.com");
    assert_eq!(req.request_uri.host, "example.com");
}

#[test]
fn parses_200_ok_with_totag() {
    let msg = parse(OK_200).expect("200 OK should parse");
    let SipMessage::Response(resp) = msg else { panic!("expected response") };
    assert_eq!(resp.status, 200);
    assert_eq!(resp.reason, "OK");
    // status > 100 ⇒ To-tag guaranteed; the refined view proves it.
    let tagged = SipResponseTagged::new(&resp).expect("200 has a To-tag");
    assert_eq!(tagged.to_tag(), "as83kf");
}

#[test]
fn in_dialog_request_exposes_infallible_tags() {
    let msg = parse(BYE).expect("BYE should parse");
    let SipMessage::Request(req) = msg else { panic!("expected request") };
    let in_dialog = InDialogRequest::new(&req).expect("BYE carries both tags");
    assert_eq!(in_dialog.from_tag(), "as83kf");
    assert_eq!(in_dialog.to_tag(), "1928");
    // Deref still exposes the base accessors.
    assert_eq!(in_dialog.method, "BYE");
}

#[test]
fn initial_invite_is_not_in_dialog() {
    let SipMessage::Request(req) = parse(INVITE).unwrap() else { unreachable!() };
    assert!(InDialogRequest::new(&req).is_err()); // no To-tag yet
}

// --- ADR-0007 strict-grammar rejections ---

#[test]
fn rejects_top_via_missing_magic_cookie() {
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=nomagic\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let err = parse(raw).unwrap_err();
    assert!(err.reason.contains("magic cookie"), "got: {}", err.reason);
}

#[test]
fn rejects_ipv4_octet_with_leading_zero() {
    // Request-URI host 10.10.10.010 — octal-confusion vector.
    let raw = b"INVITE sip:bob@10.10.10.010 SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let err = parse(raw).unwrap_err();
    assert!(err.reason.contains("leading zero"), "got: {}", err.reason);
}

#[test]
fn rejects_cseq_method_mismatch() {
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 BYE\r\n\
Content-Length: 0\r\n\r\n";
    let err = parse(raw).unwrap_err();
    assert!(err.reason.contains("does not match request method"), "got: {}", err.reason);
}

#[test]
fn rejects_non_100_response_missing_totag() {
    let raw = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let err = parse(raw).unwrap_err();
    assert!(err.reason.contains("To-tag"), "got: {}", err.reason);
}

// --- eager + non-fatal optional structured headers (ADR-0003) ---

#[test]
fn optional_headers_parsed_eagerly() {
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
P-Asserted-Identity: \"Alice\" <sip:alice@example.com>\r\n\
Diversion: <sip:div@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let SipMessage::Request(req) = parse(raw).unwrap() else { unreachable!() };
    let pai = req.optional.p_asserted_identity.as_ref().expect("PAI ok");
    assert_eq!(pai.len(), 1);
    assert_eq!(pai[0].uri, "sip:alice@example.com");
    assert_eq!(req.optional.diversion.as_ref().unwrap().len(), 1);
}

#[test]
fn malformed_optional_header_is_non_fatal_but_caught_by_validate_strict() {
    // Diversion URI `sip:@host` is malformed, but the message still parses
    // (pass-through tolerance); validate_strict() surfaces it.
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Diversion: <sip:@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let msg = parse(raw).expect("message still parses (non-fatal optional header)");
    let SipMessage::Request(ref req) = msg else { unreachable!() };
    assert!(req.optional.diversion.is_err(), "malformed Diversion captured as Err");
    assert!(msg.validate_strict().is_err(), "validate_strict surfaces it");
}

#[test]
fn rejects_invalid_numeric_header() {
    // Max-Forwards with a float — paranoid digit gate.
    let raw = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
Max-Forwards: 70.5\r\n\
From: <sip:alice@example.com>;tag=1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: x@y\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let err = parse(raw).unwrap_err();
    assert!(err.reason.contains("numeric value"), "got: {}", err.reason);
}
