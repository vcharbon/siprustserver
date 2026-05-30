//! Parser tightening: response To-tag enforcement. Port of
//! `tests/sip/parser-response-totag.test.ts`.
//!
//! RFC 3261 §8.2.6.2 / §17.1.3: every response except 100 Trying MUST carry a
//! To-tag. The parser rejects non-100 responses missing it; 100 Trying stays
//! lenient. (The TS `jssip` oracle column is dropped — see ADR-0001.)

use sip_message::{CustomParser, SipMessage, SipParser};

fn parse(raw: &str) -> Result<SipMessage, sip_message::SipParseError> {
    CustomParser::new().parse(raw.as_bytes())
}

fn response_no_tag(status: u16, reason: &str) -> String {
    format!(
        "SIP/2.0 {status} {reason}\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-x\r\n\
From: <sip:alice@example.com>;tag=from-tag-1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: notag-{status}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )
}

fn response_with_tag(status: u16, reason: &str) -> String {
    format!(
        "SIP/2.0 {status} {reason}\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-x\r\n\
From: <sip:alice@example.com>;tag=from-tag-1\r\n\
To: <sip:bob@example.com>;tag=to-tag-2\r\n\
Call-ID: tag-{status}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )
}

const STATUSES: &[(u16, &str)] = &[
    (180, "Ringing"),
    (183, "Session Progress"),
    (200, "OK"),
    (302, "Moved Temporarily"),
    (404, "Not Found"),
    (486, "Busy Here"),
    (503, "Service Unavailable"),
    (603, "Decline"),
];

#[test]
fn non_100_without_to_tag_is_rejected() {
    for &(status, reason) in STATUSES {
        let err = parse(&response_no_tag(status, reason)).unwrap_err();
        assert!(
            err.reason.contains("missing mandatory To-tag"),
            "status={status}: {}",
            err.reason
        );
        assert!(err.reason.contains(&status.to_string()), "status={status}: {}", err.reason);
    }
}

#[test]
fn non_100_with_to_tag_succeeds() {
    for &(status, reason) in STATUSES {
        let SipMessage::Response(resp) = parse(&response_with_tag(status, reason)).unwrap() else {
            panic!("expected response for status={status}");
        };
        assert_eq!(resp.to.tag.as_deref(), Some("to-tag-2"), "status={status}");
    }
}

#[test]
fn trying_100_may_omit_to_tag() {
    let raw = "SIP/2.0 100 Trying\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-100\r\n\
From: <sip:alice@example.com>;tag=from-tag-1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: trying-no-tag@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
    let SipMessage::Response(resp) = parse(raw).unwrap() else { panic!("expected response") };
    assert_eq!(resp.status, 100);
    assert_eq!(resp.to.tag, None);
}
