//! The test caller of the replicated node: the blind test caller
//! ([`b2bua_core::test_call`]) and a proxied INVITE carrying the placement
//! cookie the proxy signs, naming the call's primary and backup.

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

pub use b2bua_core::test_call::{caller_of, src, turn};

/// A proxied INVITE carrying the `w_pri`/`w_bak` cookie, keyed by Call-ID so
/// each call gets a distinct `callRef` (`{pri}|{cid}@…|alicetag`).
#[expect(clippy::unwrap_used, clippy::panic, reason = "test support: a fixed INVITE parses")]
pub fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    // RFC 3261 §25.1: a Call-ID holds one `@`, a branch none, and a URI
    // parameter's value is not empty (an empty cookie field rides as a flag).
    let call_id = if cid.contains('@') { cid.to_string() } else { format!("{cid}@10.0.0.9") };
    let branch = cid.replace('@', ".");
    let field = |name: &str, value: &str| {
        if value.is_empty() {
            format!(";{name}")
        } else {
            format!(";{name}={value}")
        }
    };
    let (w_pri, w_bak) = (field("w_pri", pri), field("w_bak", bak));
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{branch}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3{w_pri}{w_bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}
