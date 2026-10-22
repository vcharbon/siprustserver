//! A refused request carries a partial exactly when a response can answer it
//! (RFC 3261 §8.2.6, §16.3): its request line parses, it is no ACK, and every
//! Via, From, To, Call-ID and CSeq passes the full rules. The header split
//! reads past a line that breaks the grammar or a gate, so the five are still
//! read, and reports the first such line; a limit stops it with no partial.

use sip_message::{CustomParser, Method, SipParseError, SipParser, SipParserLimits};

const VIA: &str = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-refused";
const FROM: &str = "<sip:alice@example.com>;tag=a1";
const TO: &str = "<sip:bob@example.com>";
const CALL_ID: &str = "refused-1@192.0.2.10";

/// The lines of an INVITE, `extra` inserted after its Via.
fn invite_lines(request_line: &str, extra: &[&str]) -> Vec<String> {
    let mut lines = vec![request_line.to_string(), format!("Via: {VIA}")];
    lines.extend(extra.iter().map(|l| l.to_string()));
    lines.extend([
        "Max-Forwards: 70".to_string(),
        format!("From: {FROM}"),
        format!("To: {TO}"),
        format!("Call-ID: {CALL_ID}"),
        "CSeq: 1 INVITE".to_string(),
        "Contact: <sip:alice@192.0.2.10:5060>".to_string(),
        "Content-Length: 0".to_string(),
    ]);
    lines
}

fn wire(lines: &[String]) -> Vec<u8> {
    let mut out = lines.join("\r\n");
    out.push_str("\r\n\r\n");
    out.into_bytes()
}

fn invite_with(extra: &[&str]) -> Vec<u8> {
    wire(&invite_lines("INVITE sip:bob@example.com SIP/2.0", extra))
}

/// The INVITE with the line naming `name` replaced by `line`.
fn invite_replacing(name: &str, line: &str) -> Vec<u8> {
    let lines: Vec<String> = invite_lines("INVITE sip:bob@example.com SIP/2.0", &[])
        .into_iter()
        .map(|l| if l.starts_with(&format!("{name}:")) { line.to_string() } else { l })
        .collect();
    wire(&lines)
}

fn refusal(raw: &[u8]) -> SipParseError {
    match CustomParser::new().parse(raw) {
        Ok(_) => panic!("refused: {}", String::from_utf8_lossy(raw)),
        Err(e) => e,
    }
}

#[test]
fn a_well_formed_invite_parses() {
    assert!(CustomParser::new().parse(&invite_with(&[])).is_ok());
}

#[test]
fn a_bad_optional_header_leaves_the_partial() {
    let err = refusal(&invite_replacing("Max-Forwards", "Max-Forwards: abc"));
    assert_eq!(err.reason, "Invalid Max-Forwards numeric value: \"abc\"");
    let partial = err.partial().expect("the five fields read: a partial");
    assert_eq!(*partial.method(), Method::Invite);
    assert_eq!(partial.call_id().as_str(), CALL_ID);
    assert_eq!(partial.cseq().seq(), 1);
    assert_eq!(partial.from().tag(), Some("a1"));
    assert_eq!(partial.to().tag(), None);
    assert_eq!(partial.via().first().branch(), Some("z9hG4bK-refused"));
    let vias: Vec<&str> = partial.via_lines().iter().map(|v| v.as_str()).collect();
    assert_eq!(vias, [VIA]);
    assert_eq!(partial.from_line().as_str(), FROM);
    assert_eq!(partial.to_line().as_str(), TO);
    assert_eq!(partial.call_id_line().as_str(), CALL_ID);
    assert_eq!(partial.cseq_line().as_str(), "1 INVITE");
}

/// The Request-URI is not one of the five: a URI breaking §25.1 is answered.
#[test]
fn a_bad_request_uri_leaves_the_partial() {
    let err = refusal(&wire(&invite_lines("INVITE sip:b%G1ob@example.com SIP/2.0", &[])));
    assert!(err.partial().is_some(), "{err:?}");
}

/// RFC 3261 §18.3: a datagram shorter than its Content-Length is answered 400.
#[test]
fn a_body_shorter_than_its_content_length_leaves_the_partial() {
    let err = refusal(&invite_replacing("Content-Length", "Content-Length: 10"));
    assert!(err.reason.contains("exceeds remaining bytes"), "{}", err.reason);
    assert!(err.partial().is_some(), "{err:?}");
}

#[test]
fn a_bad_lower_via_leaves_no_partial() {
    let err = refusal(&invite_with(&["Via: SIP/2.0/UDP bad_host:5060;branch=z9hG4bK-lower"]));
    assert!(err.partial().is_none(), "every Via is echoed: {err:?}");
}

#[test]
fn a_bad_from_leaves_no_partial() {
    let err = refusal(&invite_replacing("From", "From: <sip:alice@example.com>junk;tag=a1"));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_bad_to_leaves_no_partial() {
    let err = refusal(&invite_replacing("To", "To: <sip:bob@exa_mple.com>"));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_bad_call_id_leaves_no_partial() {
    let err = refusal(&invite_replacing("Call-ID", "Call-ID: refused 1"));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_bad_cseq_leaves_no_partial() {
    let err = refusal(&invite_replacing("CSeq", "CSeq: x INVITE"));
    assert!(err.partial().is_none(), "{err:?}");
}

/// A From the split refuses is no From to echo, even beside a good one.
#[test]
fn a_from_line_the_split_refuses_leaves_no_partial() {
    let err = refusal(&invite_with(&["From: \"Alice <sip:alice@example.com>;tag=a2"]));
    assert!(err.reason.contains("Unterminated quoted-string"), "{}", err.reason);
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_malformed_ack_has_no_partial() {
    let lines: Vec<String> = invite_lines("ACK sip:bob@example.com SIP/2.0", &["Expires: x"])
        .into_iter()
        .map(|l| if l == "CSeq: 1 INVITE" { "CSeq: 1 ACK".to_string() } else { l })
        .collect();
    let err = refusal(&wire(&lines));
    assert!(err.partial().is_none(), "an ACK draws no response: {err:?}");
}

#[test]
fn a_malformed_response_has_no_partial() {
    let raw = format!(
        "SIP/2.0 486 Busy Here\r\nVia: {VIA}\r\nFrom: {FROM}\r\nTo: {TO};tag=b1\r\n\
         Call-ID: {CALL_ID}\r\nCSeq: 1 INVITE\r\nExpires: x\r\nContent-Length: 0\r\n\r\n"
    );
    let err = refusal(raw.as_bytes());
    assert!(err.partial().is_none(), "a response is never answered: {err:?}");
}

#[test]
fn a_bad_request_line_leaves_no_partial() {
    let err = refusal(&wire(&invite_lines("INVITE  sip:bob@example.com SIP/2.0", &[])));
    assert!(err.partial().is_none(), "{err:?}");
}

/// A line with no colon ends neither the split nor the reading of the five.
#[test]
fn the_split_reads_past_a_line_breaking_the_grammar() {
    let err = refusal(&invite_with(&["Bad Header: x"]));
    assert!(err.reason.starts_with("Expected byte 0x3a"), "{}", err.reason);
    assert!(err.partial().is_some(), "the five after the bad line still read: {err:?}");
}

/// A line whose name does not read may have been one of the five: no partial.
#[test]
fn a_line_whose_name_does_not_read_leaves_no_partial() {
    let err = refusal(&invite_with(&["X-B\u{1}ad: x"]));
    assert!(err.reason.starts_with("Illegal byte 0x1"), "{}", err.reason);
    assert!(err.partial().is_none(), "{err:?}");
}

/// The real top Via under a name that does not read, above a good Via: the
/// 400 would go up the wrong hop.
#[test]
fn a_via_whose_name_does_not_read_leaves_no_partial() {
    let lines: Vec<String> = invite_lines("INVITE sip:bob@example.com SIP/2.0", &[])
        .into_iter()
        .flat_map(|l| {
            if l.starts_with("Via:") {
                vec!["Via\u{1}: SIP/2.0/UDP 192.0.2.99;branch=z9hG4bK-real-top".to_string(), l]
            } else {
                vec![l]
            }
        })
        .collect();
    let err = refusal(&wire(&lines));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_header_name_that_is_no_token_leaves_no_partial() {
    let err = refusal(&invite_with(&["T(o: <sip:mallory@example.com>"]));
    assert!(err.reason.starts_with("Header name is not a token"), "{}", err.reason);
    assert!(err.partial().is_none(), "{err:?}");
}

/// A CSeq naming another method than the request line answers no client
/// transaction: the request is refused unanswered.
#[test]
fn a_cseq_naming_another_method_leaves_no_partial() {
    let err = refusal(&invite_replacing("CSeq", "CSeq: 1 ACK"));
    assert!(err.reason.contains("does not match request method"), "{}", err.reason);
    assert!(err.partial().is_none(), "{err:?}");
}

/// RFC 3261 §25.1: a quoted-string admits no CR or LF but its folding, which
/// the split has already unfolded.
#[test]
fn a_bare_cr_in_a_quoted_display_name_is_refused() {
    let err =
        refusal(&invite_replacing("From", "From: \"Ali\rce\" <sip:alice@example.com>;tag=a1"));
    assert!(err.partial().is_none(), "the From is not echoed: {err:?}");
}

#[test]
fn the_first_refused_line_is_the_one_reported() {
    let err = refusal(&invite_with(&["Expires: x", "Min-Expires: y"]));
    assert_eq!(err.reason, "Invalid Expires numeric value: \"x\"");
    assert!(err.partial().is_some(), "{err:?}");
}

/// The INVITE with its five fields first, then `tail`, refused by a parser
/// whose header length limit is 60.
fn limited_refusal(tail: &[&str]) -> SipParseError {
    let mut lines = vec![
        "INVITE sip:bob@example.com SIP/2.0".to_string(),
        format!("Via: {VIA}"),
        format!("From: {FROM}"),
        format!("To: {TO}"),
        format!("Call-ID: {CALL_ID}"),
        "CSeq: 1 INVITE".to_string(),
    ];
    lines.extend(tail.iter().map(|l| l.to_string()));
    lines.push("Content-Length: 0".to_string());
    let limits = SipParserLimits { max_header_length: 60, ..SipParserLimits::default() };
    CustomParser::with_limits(limits).parse(&wire(&lines)).expect_err("refused")
}

/// A header past the length limit stops the split, though the five already
/// read: a limit refusal is never answered.
#[test]
fn a_header_past_the_length_limit_leaves_no_partial() {
    let long = format!("X-Long: {}", "a".repeat(64));
    let err = limited_refusal(&[&long]);
    assert!(err.reason.contains("exceeds limit"), "{}", err.reason);
    assert!(err.partial().is_none(), "{err:?}");
}

/// Behind a refused line, the limit still stops the split unanswered; the
/// refused line stays the one reported.
#[test]
fn a_limit_behind_a_refused_line_leaves_no_partial() {
    let long = format!("X-Long: {}", "a".repeat(64));
    let err = limited_refusal(&["Expires: x", &long]);
    assert_eq!(err.reason, "Invalid Expires numeric value: \"x\"");
    assert!(err.partial().is_none(), "{err:?}");
}
