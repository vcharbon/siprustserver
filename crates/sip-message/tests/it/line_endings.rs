//! A header line ends with CRLF and nothing else (RFC 3261 §7, §25.1): a bare
//! CR or a bare LF anywhere in the header block refuses the message, with no
//! partial to answer it from, since the line structure itself is in doubt.
//! CRLF followed by SP or HTAB is a fold and reads as one SP.

use sip_message::header::HeaderName;
use sip_message::{CustomParser, SipMessage, SipParseError, SipParser};

const VIA: &str = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-endings";

/// The lines of an INVITE, `extra` inserted before its Content-Length.
fn invite_lines(extra: &[&str]) -> Vec<String> {
    let mut lines = vec![
        "INVITE sip:bob@example.com SIP/2.0".to_string(),
        format!("Via: {VIA}"),
        "Max-Forwards: 70".to_string(),
        "From: <sip:alice@example.com>;tag=a1".to_string(),
        "To: <sip:bob@example.com>".to_string(),
        "Call-ID: endings-1@192.0.2.10".to_string(),
        "CSeq: 1 INVITE".to_string(),
        "Contact: <sip:alice@192.0.2.10:5060>".to_string(),
    ];
    lines.extend(extra.iter().map(|l| l.to_string()));
    lines.push("Content-Length: 0".to_string());
    lines
}

/// `lines` each ended by `eol`, then the blank line.
fn wire_with(lines: &[String], eol: &str) -> Vec<u8> {
    let mut out = lines.join(eol);
    out.push_str(eol);
    out.push_str(eol);
    out.into_bytes()
}

fn invite_with(extra: &[&str]) -> Vec<u8> {
    wire_with(&invite_lines(extra), "\r\n")
}

/// The refusal `raw` draws; a panic naming what was read when it parses.
fn refusal(raw: &[u8]) -> SipParseError {
    match CustomParser::new().parse(raw) {
        Ok(msg) => panic!(
            "parsed with {} Via and {} header lines: {:?}",
            msg.via().len(),
            msg.headers().len(),
            String::from_utf8_lossy(raw)
        ),
        Err(e) => e,
    }
}

#[test]
fn a_crlf_message_parses() {
    assert!(CustomParser::new().parse(&invite_with(&[])).is_ok());
}

#[test]
fn an_lf_only_message_is_refused() {
    let err = refusal(&wire_with(&invite_lines(&[]), "\n"));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_cr_only_message_is_refused() {
    let err = refusal(&wire_with(&invite_lines(&[]), "\r"));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn mixed_line_endings_are_refused() {
    let lines = invite_lines(&[]);
    let mut mixed = String::new();
    for (i, line) in lines.iter().enumerate() {
        mixed.push_str(line);
        mixed.push_str(if i % 2 == 0 { "\r\n" } else { "\n" });
    }
    mixed.push_str("\r\n");
    let err = refusal(mixed.as_bytes());
    assert!(err.partial().is_none(), "{err:?}");
}

/// `\r\r` would end the header block early under a reading that takes a bare
/// CR as a line end, leaving every line after it as the body.
#[test]
fn a_doubled_cr_in_the_header_block_is_refused() {
    let err = refusal(&invite_with(&["X-Foo: a\r\rX-Bar: b"]));
    assert!(err.partial().is_none(), "{err:?}");
}

/// A bare CR inside a value would be relayed byte for byte, and a reader that
/// takes it as a line end sees a second Via.
#[test]
fn a_bare_cr_inside_a_value_is_refused() {
    let err = refusal(&invite_with(&["X-Foo: a\rVia: SIP/2.0/UDP evil.example;branch=z9hG4bKcr"]));
    assert!(err.partial().is_none(), "{err:?}");
}

#[test]
fn a_bare_lf_inside_a_value_is_refused() {
    let err = refusal(&invite_with(&["X-Foo: a\nVia: SIP/2.0/UDP evil.example;branch=z9hG4bKlf"]));
    assert!(err.partial().is_none(), "{err:?}");
}

/// A bare LF followed by SP is no fold: only CRLF folds (RFC 3261 §7.3.1).
#[test]
fn a_bare_lf_fold_is_refused() {
    refusal(&invite_with(&["Subject: first\n second"]));
}

/// The headers the parser reads lazily see no line break either: a CR inside
/// a quoted-string of one refuses the message as any bare CR does.
#[test]
fn a_cr_in_a_quoted_string_of_a_lazily_read_header_is_refused() {
    for line in [
        "P-Asserted-Identity: \"Alice\rBob\" <sip:alice@example.com>",
        "P-Asserted-Identity: \"Alice\\\rBob\" <sip:alice@example.com>",
        "Diversion: \"Alice\rBob\" <sip:alice@example.com>;reason=unknown",
        "Referred-By: \"Alice\rBob\" <sip:alice@example.com>",
        "Record-Route: \"Alice\rBob\" <sip:p@192.0.2.20;lr>",
        "Reason: SIP;cause=200;text=\"Ca\rll\"",
        "Reason: SIP;cause=200;text=\"Ca\\\rll\"",
    ] {
        let err = refusal(&invite_with(&[line]));
        assert!(err.partial().is_none(), "{line:?}: {err:?}");
    }
}

#[test]
fn a_crlf_fold_still_reads_as_one_space() {
    let msg = CustomParser::new()
        .parse(&invite_with(&["Subject: first\r\n second\r\n\tthird"]))
        .expect("a CRLF fold parses");
    let subject: Vec<&str> = match &msg {
        SipMessage::Request(r) => r.raw(HeaderName::Subject).collect(),
        SipMessage::Response(r) => r.raw(HeaderName::Subject).collect(),
    };
    assert_eq!(subject, ["first second third"]);
}

/// The fragment reading (RFC 3420) holds the same line rule.
#[test]
fn a_fragment_with_a_bare_lf_is_refused() {
    let raw = wire_with(&invite_lines(&[]), "\n");
    assert!(CustomParser::new().parse_header_fields(&raw).is_err());
}
