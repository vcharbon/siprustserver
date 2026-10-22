//! Pins [`intake_shed_hook`]: the hook is pure over `(raw, depth)`
//! — no socket needed — so each case invokes it directly.

use super::*;
use std::net::SocketAddr;

const WATERMARK: usize = 8;

fn src() -> SocketAddr {
    "192.0.2.9:5060".parse().unwrap()
}

/// The hook's verdict on `raw` arriving at queue depth `depth`.
fn judge(hook: &PreIngressHook, raw: &[u8], depth: usize) -> PreIngressAction {
    hook(raw, src(), depth)
}

/// A new INVITE whose To header line is `to`.
fn invite_with_to(to: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@192.0.2.20:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-shape\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
{to}\r\n\
Call-ID: shape@192.0.2.10\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

const NEW_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const IN_DIALOG_INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-re\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n";

const EMERGENCY_INVITE: &[u8] = b"INVITE sip:911@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-em\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:911@example.com>\r\n\
Resource-Priority: esnet.0\r\n\
Call-ID: em@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

const BYE: &[u8] = b"BYE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bye\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 3 BYE\r\nContent-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>;tag=b2\r\n\
Call-ID: shed@10.0.0.1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";

#[test]
fn below_the_watermark_everything_is_admitted() {
    let hook = intake_shed_hook(WATERMARK);
    for raw in [NEW_INVITE, IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(judge(&hook, raw, WATERMARK - 1), PreIngressAction::Accept);
    }
}

#[test]
fn at_the_watermark_only_the_new_non_emergency_invite_is_dropped() {
    let hook = intake_shed_hook(WATERMARK);
    assert_eq!(judge(&hook, NEW_INVITE, WATERMARK), PreIngressAction::Drop);
    for admitted in [IN_DIALOG_INVITE, EMERGENCY_INVITE, BYE, OK_200] {
        assert_eq!(judge(&hook, admitted, WATERMARK), PreIngressAction::Accept);
    }
}

/// The shed reads the To-tag exactly as the parser does (RFC 3261 §20.39,
/// §25.1): a `tag=` inside the display name or the URI is no tag, and a tag
/// parameter written with linear whitespace around `;` and `=`, or folded
/// onto a continuation line, is one.
#[test]
fn the_shed_reads_the_to_tag_as_the_parser_does() {
    use sip_message::{CustomParser, SipMessage, SipParser};
    let hook = intake_shed_hook(WATERMARK);
    let mut misread = Vec::new();
    for (to, tag, verdict) in [
        ("To: \"tag=FAKE\" <sip:bob@example.com>", None, PreIngressAction::Drop),
        ("To: <sip:bob@example.com;tag=URI>", None, PreIngressAction::Drop),
        ("To: <sip:bob@example.com>; tag = REAL", Some("REAL"), PreIngressAction::Accept),
        ("To: <sip:bob@example.com>\r\n ;tag=REAL", Some("REAL"), PreIngressAction::Accept),
    ] {
        let raw = invite_with_to(to);
        let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&raw) else {
            panic!("the parser reads {to:?}");
        };
        assert_eq!(req.to().tag(), tag, "the parser's To-tag of {to:?}");
        let judged = judge(&hook, &raw, WATERMARK);
        if judged != verdict {
            misread.push(format!("{to:?}: {judged:?}, not {verdict:?}"));
        }
    }
    assert!(misread.is_empty(), "the shed misreads the To-tag of {misread:#?}");
}

#[test]
fn deeper_than_the_watermark_still_sheds_new_invites() {
    let hook = intake_shed_hook(WATERMARK);
    assert_eq!(judge(&hook, NEW_INVITE, WATERMARK * 100), PreIngressAction::Drop);
}

#[test]
fn watermark_is_the_percentage_of_the_queue_and_floors_at_one() {
    assert_eq!(intake_shed_watermark(8192, 50), 4096);
    assert_eq!(intake_shed_watermark(1, 50), 1);
    assert_eq!(intake_shed_watermark(8192, 0), 1);
}
