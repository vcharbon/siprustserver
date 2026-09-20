//! The body bound of a message that states no `Content-Length`: over a
//! datagram transport the body is the remainder of the datagram (RFC 3261
//! §18.3); over a stream transport the header is mandatory (§20.14) and the
//! message is refused. A declared length bounds the body whatever the framing.

use sip_message::{CustomParser, Framing, SipMessage, SipParser, SipParserLimits};

const HEAD: &str = "INFO sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP a.invalid;branch=z9hG4bK1\r\nFrom: <sip:a@h>;tag=a1\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c1@a.invalid\r\nCSeq: 2 INFO\r\nMax-Forwards: 70\r\nContent-Type: text/plain\r\n";

fn wire(content_length: Option<usize>, tail: &[u8]) -> Vec<u8> {
    let mut out = HEAD.to_string();
    if let Some(len) = content_length {
        out.push_str(&format!("Content-Length: {len}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(tail);
    out
}

fn parser(framing: Framing) -> CustomParser {
    CustomParser::with_limits(SipParserLimits { framing, ..SipParserLimits::default() })
}

fn body_of(msg: SipMessage) -> Vec<u8> {
    match msg {
        SipMessage::Request(req) => req.body().to_vec(),
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

#[test]
fn a_datagram_without_content_length_carries_the_remainder_as_its_body() {
    let msg = parser(Framing::Datagram).parse(&wire(None, b"hello")).expect("parses");
    assert_eq!(body_of(msg), b"hello");
}

#[test]
fn the_default_framing_is_datagram() {
    let msg = CustomParser::new().parse(&wire(None, b"hello")).expect("parses");
    assert_eq!(body_of(msg), b"hello");
}

#[test]
fn a_datagram_without_content_length_and_no_tail_is_bodiless() {
    let msg = parser(Framing::Datagram).parse(&wire(None, b"")).expect("parses");
    assert!(body_of(msg).is_empty());
}

#[test]
fn a_stream_message_without_content_length_is_refused() {
    let err = parser(Framing::Stream).parse(&wire(None, b"hello")).expect_err("refused");
    assert!(err.to_string().contains("Content-Length"), "{err}");
}

#[test]
fn a_declared_length_bounds_the_body_whatever_the_framing() {
    for framing in [Framing::Datagram, Framing::Stream] {
        let msg = parser(framing).parse(&wire(Some(5), b"hello\r\n")).expect("parses");
        assert_eq!(body_of(msg), b"hello", "{framing:?}");
        let msg = parser(framing).parse(&wire(Some(0), b"hello")).expect("parses");
        assert!(body_of(msg).is_empty(), "{framing:?}: a declared zero states no body");
        let err = parser(framing).parse(&wire(Some(9), b"hello")).expect_err("short");
        assert!(err.to_string().contains("exceeds"), "{framing:?}: {err}");
    }
}
