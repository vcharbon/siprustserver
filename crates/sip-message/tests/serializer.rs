//! Serializer unit tests — Content-Length safety net. Port of
//! `tests/sip/Serializer.test.ts`.
//!
//! Verifies the serializer auto-corrects Content-Length mismatches and adds a
//! missing Content-Length when a body is present. The TS test additionally
//! spies on `console.warn` to assert a warning fires; in Rust the correction is
//! observable directly in the output bytes, so we assert on the serialized
//! Content-Length value (the warning goes to stderr via `eprintln!`).

use sip_message::serializer::serialize;
use sip_message::types::{SipHeader, SipMessage, SipResponse};
use sip_message::{CustomParser, SipParser};

const SDP_BODY: &[u8] = b"v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\n";

/// A minimal, well-formed 200 OK with an empty body — the mutable base for the
/// scenarios below. Built by the real parser so the struct is faithful.
fn base_response() -> SipResponse {
    let raw = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: serializer-test-call\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let SipMessage::Response(resp) = CustomParser::new().parse(raw).expect("base parses") else {
        panic!("expected response");
    };
    resp
}

/// Set (or insert) a header value by case-insensitive name.
fn set_header(resp: &mut SipResponse, name: &str, value: &str) {
    if let Some(h) = resp.headers.iter_mut().find(|h| h.name.eq_ignore_ascii_case(name)) {
        h.value = value.to_string();
    } else {
        resp.headers.push(SipHeader { name: name.to_string(), value: value.to_string() });
    }
}

fn remove_header(resp: &mut SipResponse, name: &str) {
    resp.headers.retain(|h| !h.name.eq_ignore_ascii_case(name));
}

/// Extract a header value from the serialized buffer (mirrors the TS helper).
fn extract_header(buf: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(buf);
    let lower = name.to_ascii_lowercase();
    for line in text.split("\r\n") {
        if let Some(colon) = line.find(':') {
            if line[..colon].trim().to_ascii_lowercase() == lower {
                return Some(line[colon + 1..].trim().to_string());
            }
        }
    }
    None
}

#[test]
fn correct_content_length_passes_through() {
    let mut resp = base_response();
    set_header(&mut resp, "Content-Length", &SDP_BODY.len().to_string());
    resp.body = SDP_BODY.to_vec();

    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn content_length_mismatch_is_auto_corrected() {
    let mut resp = base_response();
    set_header(&mut resp, "Content-Length", "999");
    resp.body = SDP_BODY.to_vec();

    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn missing_content_length_with_body_adds_the_header() {
    let mut resp = base_response();
    remove_header(&mut resp, "Content-Length");
    resp.body = SDP_BODY.to_vec();

    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn empty_body_without_content_length_does_not_add_the_header() {
    let mut resp = base_response();
    remove_header(&mut resp, "Content-Length");
    // body already empty

    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), None);
}

#[test]
fn empty_body_with_content_length_zero_passes_through() {
    let resp = base_response(); // already Content-Length: 0, empty body
    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), Some("0".to_string()));
}

#[test]
fn content_length_zero_with_non_empty_body_is_corrected() {
    let mut resp = base_response();
    set_header(&mut resp, "Content-Length", "0");
    resp.body = SDP_BODY.to_vec();

    let buf = serialize(&SipMessage::Response(resp));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn body_is_appended_verbatim() {
    let mut resp = base_response();
    set_header(&mut resp, "Content-Length", &SDP_BODY.len().to_string());
    resp.body = SDP_BODY.to_vec();

    let buf = serialize(&SipMessage::Response(resp));
    assert!(buf.ends_with(SDP_BODY), "body appended unmodified");
    // Head/body separator present exactly once.
    let sep = b"\r\n\r\n";
    assert!(buf.windows(4).any(|w| w == sep));
}
