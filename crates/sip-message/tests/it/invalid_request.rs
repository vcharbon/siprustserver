//! The stateless answer to a refused request (RFC 3261 §8.2.6): `400 Invalid
//! Request`, Via / From / To / Call-ID / CSeq echoed as received, a To-tag
//! added when the To has none, and a Warning naming the refusal whose
//! warn-text keeps letters, digits, space and plain punctuation, every other
//! byte replaced, and is capped.

use sip_message::generators::generate_invalid_request;
use sip_message::header::{warning_value, WARN_TEXT_MAX};
use sip_message::{CustomParser, HeaderName, SipMessage, SipParseError, SipParser};

const AGENT: &str = "192.0.2.1:5060";

fn refusal(to: &str) -> SipParseError {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-refused;rport\r\n\
         v: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-lower\r\n\
         Max-Forwards: abc\r\n\
         From: \"Alice\" <sip:alice@example.com>;tag=a1\r\n\
         To: {to}\r\n\
         Call-ID: refused-1@192.0.2.10\r\n\
         CSeq: 7 INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).expect_err("Max-Forwards refuses it")
}

fn answer(to: &str) -> Vec<u8> {
    let err = refusal(to);
    let partial = err.partial().expect("the five read: a partial");
    let source = ("198.51.100.7".to_string(), 40000);
    generate_invalid_request(partial, &err.reason, AGENT, "t400", &source)
        .expect("built")
        .image()
        .to_vec()
}

/// The top Via records the packet's source (RFC 3261 §18.2.1, RFC 3581 §4).
#[test]
fn the_400_echoes_the_five_and_names_the_refusal() {
    let image = String::from_utf8(answer("<sip:bob@example.com>")).expect("text");
    assert_eq!(
        image,
        "SIP/2.0 400 Invalid Request\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-refused;rport=40000;received=198.51.100.7\r\n\
         Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-lower\r\n\
         From: \"Alice\" <sip:alice@example.com>;tag=a1\r\n\
         To: <sip:bob@example.com>;tag=t400\r\n\
         Call-ID: refused-1@192.0.2.10\r\n\
         CSeq: 7 INVITE\r\n\
         Warning: 399 192.0.2.1:5060 \"Invalid Max-Forwards numeric value: ?abc?\"\r\n\
         Content-Length: 0\r\n\r\n"
    );
}

#[test]
fn a_tagged_to_is_echoed_as_received() {
    let image = String::from_utf8(answer("<sip:bob@example.com>;tag=b9")).expect("text");
    assert!(image.contains("\r\nTo: <sip:bob@example.com>;tag=b9\r\n"), "{image}");
}

#[test]
fn the_400_parses_as_a_response() {
    match CustomParser::new().parse(&answer("<sip:bob@example.com>")) {
        Ok(SipMessage::Response(resp)) => {
            assert_eq!(resp.status(), 400);
            assert_eq!(resp.to().tag(), Some("t400"));
            assert_eq!(resp.raw_text(HeaderName::Warning).count(), 1);
        }
        other => panic!("a response: {other:?}"),
    }
}

/// The text between the warn-text's quotes.
fn warn_text(value: &str) -> &str {
    let open = value.find('"').expect("an opening quote");
    value.get(open + 1..value.len() - 1).expect("quoted")
}

#[test]
fn letters_digits_space_and_plain_punctuation_are_kept() {
    let kept = "Az09 .,:;-_()/=+'@[]<>";
    assert_eq!(warning_value(399, AGENT, kept), format!("399 192.0.2.1:5060 \"{kept}\""));
}

/// No byte of the reason can close the quoted-string or the header line.
#[test]
fn crlf_and_quotes_cannot_break_out_of_the_header() {
    let value = warning_value(399, AGENT, "a\"\r\nVia: x\\\"");
    assert_eq!(value, "399 192.0.2.1:5060 \"a???Via: x??\"");
    assert_eq!(value.matches('"').count(), 2);
    assert!(!value.contains('\r') && !value.contains('\n'));
}

/// `%`, `\`, controls and every byte past ASCII become `?`, one per byte.
#[test]
fn every_other_byte_becomes_a_question_mark() {
    assert_eq!(warning_value(399, AGENT, "zo\u{eb}\t%0D\\~"), "399 192.0.2.1:5060 \"zo????0D??\"");
}

#[test]
fn the_warn_text_is_capped() {
    let plain = warning_value(399, AGENT, &"a".repeat(300));
    assert_eq!(warn_text(&plain).len(), WARN_TEXT_MAX);
    let accents = warning_value(399, AGENT, &"\u{eb}".repeat(300));
    assert_eq!(warn_text(&accents), "?".repeat(WARN_TEXT_MAX));
}
