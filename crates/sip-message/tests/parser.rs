//! RFC 4475 torture corpus — field-extraction + eager-leniency assertions.
//! Port of `tests/sip/Parser.test.ts`.
//!
//! The accept/reject verdicts overlap `compliance_matrix.rs`; what this file
//! adds is (a) assertions on the *parsed field content* of the messages that
//! parse, and (b) the eager-only §3.1.2 leniency split — `parse()` alone
//! accepts the Date / bare-addr-spec / non-token-display cases that
//! `validate_strict()` (exercised in the compliance matrix) rejects.

use std::fs;
use std::path::PathBuf;

use sip_message::message_helpers::get_header;
use sip_message::{CustomParser, SipMessage, SipParser};

fn fixture(category: &str, name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(category);
    p.push(format!("{name}.sip"));
    fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

fn parse(category: &str, name: &str) -> Result<SipMessage, sip_message::SipParseError> {
    CustomParser::new().parse(&fixture(category, name))
}

fn request(category: &str, name: &str) -> sip_message::SipRequest {
    match parse(category, name).unwrap_or_else(|e| panic!("{name} should parse: {}", e.reason)) {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("{name}: expected request"),
    }
}

// --- §3.1.1 valid messages: parsed field content ---

#[test]
fn v3_1_1_3_valid_percent_escaping() {
    let req = request("rfc4475-valid", "validPercentEscaping");
    assert_eq!(req.method, "INVITE");
    assert!(req.uri.contains("example.net"), "uri: {}", req.uri);
}

#[test]
fn v3_1_1_4_escaped_nulls() {
    assert_eq!(request("rfc4475-valid", "escapedNulls").method, "REGISTER");
}

#[test]
fn v3_1_1_5_percent_not_escape() {
    assert!(!request("rfc4475-valid", "percentNotEscape").method.is_empty());
}

#[test]
fn v3_1_1_6_no_lws_before_angle_bracket() {
    assert_eq!(request("rfc4475-valid", "noLwsBeforeAngleBracket").method, "OPTIONS");
}

#[test]
fn v3_1_1_8_extra_trailing_octets() {
    assert_eq!(request("rfc4475-valid", "extraTrailingOctets").method, "REGISTER");
}

#[test]
fn v3_1_1_9_semicolon_in_user_part() {
    let req = request("rfc4475-valid", "semicolonInUserPart");
    assert_eq!(req.method, "OPTIONS");
    assert!(req.uri.contains("example.com"), "uri: {}", req.uri);
}

#[test]
fn v3_1_1_11_multipart_mime() {
    let req = request("rfc4475-valid", "multipartMime");
    assert_eq!(req.method, "MESSAGE");
    let ct = get_header(&req.headers, "Content-Type").unwrap_or("");
    assert!(ct.contains("multipart/mixed"), "Content-Type: {ct}");
    assert!(!req.body.is_empty(), "body should be non-empty");
}

// --- §3.1.1 valid-per-RFC but rejected by ADR-0007 ---

#[test]
fn v3_1_1_7_long_values_rejected_by_adr_0007() {
    assert!(parse("rfc4475-valid", "longValues").is_err());
}

#[test]
fn v3_1_1_10_varied_transports_rejected_by_adr_0007() {
    assert!(parse("rfc4475-valid", "variedTransports").is_err());
}

// --- §3.1.2 invalid messages the eager parser REJECTS ---

#[test]
fn invalid_messages_rejected_by_eager_parse() {
    let rejected = [
        "extraneousSeparators",     // 3.1.2.1
        "contentLengthTooLarge",    // 3.1.2.2
        "negativeContentLength",    // 3.1.2.3
        "overlargRequestScalars",   // 3.1.2.4
        "overlargResponseScalars",  // 3.1.2.5
        "unterminatedQuotedString", // 3.1.2.6
        "angleBracketRequestUri",   // 3.1.2.7
        "embeddedLwsInUri",         // 3.1.2.8
        "multipleSPInRequestLine",  // 3.1.2.9
        "spacesInAddrSpec",         // 3.1.2.14
        "unknownProtocolVersion",   // 3.1.2.16
        "methodMismatch",           // 3.1.2.17
        "unknownMethodMismatch",    // 3.1.2.18
        "overlargeResponseCode",    // 3.1.2.19
    ];
    for name in rejected {
        assert!(parse("rfc4475-invalid", name).is_err(), "{name} should be rejected");
    }
}

// --- §3.1.2 cases the EAGER parser accepts (validate_strict rejects them; see
//     compliance_matrix.rs) — documents the tolerant-parse / strict-validate split ---

#[test]
fn invalid_messages_accepted_eagerly_as_request() {
    let accepted = [
        "trailingSpacesInRequestLine", // 3.1.2.10
        "escapedHeadersInUri",         // 3.1.2.11
        "invalidTimezone",             // 3.1.2.12
        "unencloseNameAddr",           // 3.1.2.13
        "nonTokenDisplayName",         // 3.1.2.15
    ];
    for name in accepted {
        let msg = parse("rfc4475-invalid", name)
            .unwrap_or_else(|e| panic!("{name} should parse eagerly: {}", e.reason));
        assert!(matches!(msg, SipMessage::Request(_)), "{name}: expected request");
    }
}
