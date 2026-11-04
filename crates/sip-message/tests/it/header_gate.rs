//! The header gate: a numeric header is its digits alone (ADR-0001), a header
//! the grammar gives one value appears once (RFC 3261 §7.3.1), a control byte
//! stands in a value only as a quoted-pair (§25.1), and the header block is
//! UTF-8. A refused line in a request whose five echoed fields read leaves the
//! partial a 400 answers with (ADR-0054); a block that is not UTF-8 leaves
//! none.

use sip_message::generators::generate_invalid_request;
use sip_message::{CustomParser, SipMessage, SipParseError, SipParser};

const HEAD: &str = "OPTIONS sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-gate\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: gate-1@192.0.2.10\r\n\
CSeq: 1 OPTIONS\r\n";

/// The OPTIONS with `lines` after its five echoed fields, then `body`.
fn options(lines: &[&[u8]], body: &[u8]) -> Vec<u8> {
    let mut out = HEAD.as_bytes().to_vec();
    for line in lines {
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn parse(raw: &[u8]) -> Result<SipMessage, SipParseError> {
    CustomParser::new().parse(raw)
}

/// The refusal of `raw`, which must carry the partial a 400 answers with.
fn answered(raw: &[u8]) -> String {
    match parse(raw) {
        Ok(_) => panic!("refused: {:?}", String::from_utf8_lossy(raw)),
        Err(e) => {
            assert!(e.partial().is_some(), "answerable: {}", e.reason);
            e.reason
        }
    }
}

fn accepted(raw: &[u8]) -> SipMessage {
    parse(raw).unwrap_or_else(|e| panic!("{:?}: {}", String::from_utf8_lossy(raw), e.reason))
}

#[test]
fn a_numeric_value_is_its_digits_alone() {
    for line in [
        "Max-Forwards: 70 junk",
        "Expires: 3600 x",
        "Min-Expires: 60 x",
        "Session-Expires: 1800 x;refresher=uac",
        "Min-SE: 90 x",
        "x: 1800 junk",
    ] {
        let raw = options(&[line.as_bytes(), b"Content-Length: 0"], b"");
        let reason = answered(&raw);
        assert!(reason.contains("numeric value"), "{line}: {reason}");
    }
    let raw = options(&[b"Max-Forwards: 70", b"Content-Length: 5 99"], b"hello");
    assert!(answered(&raw).contains("Invalid Content-Length numeric value"));
}

#[test]
fn a_numeric_value_keeps_the_tail_its_grammar_has() {
    for line in [
        "Session-Expires: 1800;refresher=uac",
        "Session-Expires: 1800 ;refresher=uac",
        "Min-SE: 90",
        "Expires: 3600",
    ] {
        accepted(&options(&[b"Max-Forwards: 70", line.as_bytes(), b"Content-Length: 0"], b""));
    }
}

#[test]
fn a_second_content_length_is_refused() {
    let raw =
        options(&[b"Max-Forwards: 70", b"Content-Length: 13", b"Content-Length: 5"], b"hello");
    assert!(answered(&raw).contains("Content-Length"));
    // The same value twice, one row in compact form, is still two values.
    let raw = options(&[b"Max-Forwards: 70", b"Content-Length: 0", b"l: 0"], b"");
    assert!(answered(&raw).contains("Content-Length"));
}

#[test]
fn a_second_max_forwards_is_refused() {
    let raw = options(&[b"Max-Forwards: 70", b"Max-Forwards: 5", b"Content-Length: 0"], b"");
    assert!(answered(&raw).contains("Max-Forwards"));
}

#[test]
fn a_bare_control_byte_in_a_value_is_refused() {
    for ctl in [0x00u8, 0x01, 0x07, 0x0b, 0x1f, 0x7f] {
        for (prefix, suffix) in [
            (&b"X-Note: a"[..], &b"b"[..]),
            (b"Subject: a", b"b"),
            (b"X-Note: \"a", b"b\""),
            (b"X-Note: a\\", b"b"),
        ] {
            let line = [prefix, &[ctl], suffix].concat();
            let raw = options(&[b"Max-Forwards: 70", &line, b"Content-Length: 0"], b"");
            let reason = answered(&raw);
            assert!(reason.contains("control byte"), "{line:?}: {reason}");
        }
    }
}

#[test]
fn a_control_byte_in_an_echoed_field_leaves_no_partial() {
    let raw = HEAD.replace("tag=a1", "tag=a1;x=\"a\0b\"") + "Max-Forwards: 70\r\n\r\n";
    let Err(err) = parse(raw.as_bytes()) else { panic!("refused") };
    assert!(err.partial().is_none(), "{}", err.reason);
}

#[test]
fn a_quoted_pair_carries_a_control_byte() {
    // RFC 4475 §3.1.1.2: a quoted-pair escapes NUL, BEL and DEL.
    let to = b"X-Note: \"BEL:\\\x07 NUL:\\\x00 DEL:\\\x7f\"";
    let msg = accepted(&options(&[b"Max-Forwards: 70", to, b"Content-Length: 0"], b""));
    let note = msg.headers().iter().find(|h| h.name.as_str() == "X-Note").expect("kept");
    assert_eq!(note.value.as_bytes(), &to[8..], "carried as received");
    // A comment carries a quoted-pair too, where the grammar has comments.
    let server = b"Server: stack/1 (a \\\x00 b)";
    accepted(&options(&[b"Max-Forwards: 70", server, b"Content-Length: 0"], b""));
    // HTAB is LWS, no control byte.
    accepted(&options(&[b"Max-Forwards: 70", b"Subject: a\tb", b"Content-Length: 0"], b""));
}

#[test]
fn a_reason_phrase_holds_no_control_byte() {
    let raw = b"SIP/2.0 200 O\x00K\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-gate\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b1\r\n\
Call-ID: gate-1@192.0.2.10\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n";
    assert!(parse(raw).is_err(), "a NUL in the reason phrase is refused");
}

/// A header block that is not UTF-8 (RFC 3629) is refused, never rewritten;
/// the line holding the byte is a refused line, so a request whose five
/// echoed fields read is answered 400, those fields echoed byte for byte.
#[test]
fn a_header_block_that_is_not_utf8_is_refused_and_answered() {
    let raw = options(&[b"Max-Forwards: 70", b"Subject: caf\xe9", b"Content-Length: 0"], b"");
    let Err(err) = parse(&raw) else { panic!("refused, never rewritten") };
    assert!(err.reason.contains("UTF-8"), "{}", err.reason);
    let partial = err.partial().expect("the five read: a partial");
    let source = ("198.51.100.7".to_string(), 40000);
    let answer = generate_invalid_request(partial, &err.reason, "192.0.2.1:5060", "t400", &source)
        .expect("built");
    let image = String::from_utf8(answer.image().to_vec()).expect("text");
    assert!(image.starts_with("SIP/2.0 400 Invalid Request\r\n"), "{image}");
    for line in [
        "\r\nFrom: <sip:alice@example.com>;tag=a1\r\n",
        "\r\nTo: <sip:bob@example.com>;tag=t400\r\n",
        "\r\nCall-ID: gate-1@192.0.2.10\r\n",
        "\r\nCSeq: 1 OPTIONS\r\n",
    ] {
        assert!(image.contains(line), "{line:?} in {image}");
    }
    // A byte that is no UTF-8 inside an echoed field leaves no partial.
    let (before, after) = HEAD.split_once("tag=a1").expect("the From tag");
    let raw =
        [before.as_bytes(), b"tag=a\xe91", after.as_bytes(), b"Max-Forwards: 70\r\n\r\n"].concat();
    let Err(err) = parse(&raw) else { panic!("refused") };
    assert!(err.partial().is_none(), "{}", err.reason);
    // The body is bytes, never text: a binary body parses.
    let raw = options(&[b"Max-Forwards: 70", b"Content-Length: 2"], b"\xe9\xff");
    assert_eq!(accepted(&raw).body().as_ref(), b"\xe9\xff");
}

/// A comment carries a quoted-pair only where the header's grammar has one:
/// Server, User-Agent, Retry-After. Comments nest, and a closed comment
/// grants nothing after it.
#[test]
fn a_comment_grants_a_quoted_pair_only_where_the_grammar_has_one() {
    let with = |line: &[u8]| options(&[b"Max-Forwards: 70", line, b"Content-Length: 0"], b"");
    assert!(answered(&with(b"Subject: (a \\\x00 b)")).contains("control byte"));
    accepted(&with(b"Server: stack/1 (a (b \\\x00) c)"));
    accepted(&with(b"User-Agent: stack/1 (a (b) \\\x00)"));
    assert!(answered(&with(b"Server: stack/1 (a) \\\x00")).contains("control byte"));
}
