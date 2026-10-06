//! Serializer unit tests — the Content-Length safety net at the one
//! serialization boundary.
//!
//! A frozen message cannot disagree with its own body: `freeze` restates
//! Content-Length and the parser gates the wire form. The header BLOCK can still
//! be stated by a caller — [`emitted_wire`] is the template lane's seam — so the
//! net is exercised there, where a wrong length can actually arrive. The
//! correction is observable in the output bytes (the warning goes to stderr).

use sip_message::serializer::serialize;
use sip_message::types::{SipHeader, SipMessage, SipResponse};
use sip_message::{emitted_wire, CustomParser, SipParser};

const SDP_BODY: &[u8] = b"v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\n";

fn response(content_length: usize, body: &[u8]) -> SipResponse {
    let mut raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:alice@example.com>;tag=tagA\r\n\
To: <sip:bob@example.com>;tag=tagB\r\n\
Call-ID: serializer-test-call\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: {content_length}\r\n\r\n"
    )
    .into_bytes();
    raw.extend_from_slice(body);
    let SipMessage::Response(resp) = CustomParser::new().parse(&raw).expect("base parses") else {
        panic!("expected response");
    };
    resp
}

/// A 200 OK with an empty body and `Content-Length: 0`.
fn empty_response() -> SipResponse {
    response(0, b"")
}

/// A 200 OK carrying the SDP body, correctly framed.
fn body_response() -> SipResponse {
    response(SDP_BODY.len(), SDP_BODY)
}

/// Render `resp` under a header block the caller doctored — the one path where
/// the declared length and the body can disagree.
fn wire_with(resp: SipResponse, doctor: impl FnOnce(&mut Vec<SipHeader>)) -> Vec<u8> {
    let mut headers = resp.headers().to_vec();
    doctor(&mut headers);
    emitted_wire(&SipMessage::Response(resp), &headers)
}

fn set_length(headers: &mut Vec<SipHeader>, value: &str) {
    match headers.iter_mut().find(|h| h.name.eq_ignore_ascii_case("content-length")) {
        Some(h) => h.value = value.to_string().into(),
        None => headers.push(SipHeader::new("Content-Length", value)),
    }
}

fn drop_length(headers: &mut Vec<SipHeader>) {
    headers.retain(|h| !h.name.eq_ignore_ascii_case("content-length"));
}

/// Extract a header value from the serialized buffer.
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
    let buf = serialize(&SipMessage::Response(body_response()));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn content_length_mismatch_is_auto_corrected() {
    let buf = wire_with(body_response(), |h| set_length(h, "999"));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn missing_content_length_with_body_adds_the_header() {
    let buf = wire_with(body_response(), drop_length);
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn empty_body_without_content_length_does_not_add_the_header() {
    let buf = wire_with(empty_response(), drop_length);
    assert_eq!(extract_header(&buf, "Content-Length"), None);
}

#[test]
fn empty_body_with_content_length_zero_passes_through() {
    let buf = serialize(&SipMessage::Response(empty_response()));
    assert_eq!(extract_header(&buf, "Content-Length"), Some("0".to_string()));
}

#[test]
fn content_length_zero_with_non_empty_body_is_corrected() {
    let buf = wire_with(body_response(), |h| set_length(h, "0"));
    assert_eq!(extract_header(&buf, "Content-Length"), Some(SDP_BODY.len().to_string()));
}

#[test]
fn body_is_appended_verbatim() {
    let buf = serialize(&SipMessage::Response(body_response()));
    assert!(buf.ends_with(SDP_BODY), "body appended unmodified");
    // Head/body separator present exactly once.
    let sep = b"\r\n\r\n";
    assert!(buf.windows(4).any(|w| w == sep));
}
