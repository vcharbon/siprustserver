//! Header line folding (RFC 3261 §7.3.1): a continuation line (CRLF followed by
//! SP or HTAB) joins the previous line, the fold reading as one SP. Unfolding
//! costs constant stack and work linear in the bytes read, whatever the number
//! of continuation lines.

use sip_message::header::HeaderName;
use sip_message::{CustomParser, SipMessage, SipParser};

fn invite_with_subject(subject: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@192.0.2.20 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-unfold\r\n\
From: <sip:alice@192.0.2.10>;tag=fromtag\r\n\
To: <sip:bob@192.0.2.20>\r\n\
Call-ID: unfold@192.0.2.10\r\n\
CSeq: 1 INVITE\r\n\
Max-Forwards: 70\r\n\
Subject: {subject}\r\n\
Contact: <sip:alice@192.0.2.10:5060>\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn subject(msg: &SipMessage) -> Vec<&str> {
    msg.raw(HeaderName::of(&sip_message::SipStr::owned("subject"))).collect()
}

#[test]
fn a_fold_reads_as_one_space_and_the_next_header_follows() {
    let msg = CustomParser::new()
        .parse(&invite_with_subject("first\r\n second\r\n\tthird"))
        .expect("parse");
    assert_eq!(subject(&msg), vec!["first second third"]);
    assert_eq!(
        msg.raw(HeaderName::Contact).collect::<Vec<_>>(),
        vec!["<sip:alice@192.0.2.10:5060>"]
    );
}

#[test]
fn a_bare_lf_fold_reads_as_one_space() {
    let msg = CustomParser::new().parse(&invite_with_subject("first\n second")).expect("parse");
    assert_eq!(subject(&msg), vec!["first second"]);
}

#[test]
fn whitespace_past_the_folds_first_is_kept() {
    let msg = CustomParser::new().parse(&invite_with_subject("first\r\n   second")).expect("parse");
    assert_eq!(subject(&msg), vec!["first   second"]);
}

#[test]
fn trailing_empty_continuation_lines_trim_away() {
    let msg = CustomParser::new().parse(&invite_with_subject("only\r\n \r\n\t")).expect("parse");
    assert_eq!(subject(&msg), vec!["only"]);
}

/// A datagram's worth of empty continuation lines: each must not cost a stack
/// frame, so the parse completes on a thread whose stack is far smaller than
/// one frame per fold would need.
#[test]
fn twenty_thousand_folds_parse_on_a_small_stack() {
    let mut value = String::from("x");
    for _ in 0..20_000 {
        value.push_str("\r\n ");
    }
    let raw = invite_with_subject(&value);
    let parsed = std::thread::Builder::new()
        .stack_size(128 * 1024)
        .spawn(move || {
            let msg = CustomParser::new().parse(&raw).expect("parse");
            subject(&msg).iter().map(|v| v.to_string()).collect::<Vec<_>>()
        })
        .expect("spawn")
        .join()
        .expect("parse thread");
    assert_eq!(parsed, vec!["x"]);
}
