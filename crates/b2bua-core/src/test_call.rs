//! The test caller every decision test builds its call from: a proxied
//! INVITE, the address it arrives from, the INVITE a call's record rebuilds,
//! and a turn under the fixed test key.

use std::net::SocketAddr;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

/// A turn under the fixed test key, numbered `turn_seq`.
pub fn turn(turn_seq: u64) -> crate::turn_scope::TurnScope {
    crate::turn_scope::TurnScope::of(sip_ids::TurnIds::fixed(turn_seq))
}

/// The caller's INVITE `call`'s record rebuilds.
#[expect(clippy::expect_used, reason = "test support: a test call's record rebuilds its INVITE")]
pub fn caller_of(call: &call::Call) -> SipRequest {
    crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite)
        .expect("the test call's record rebuilds its caller's INVITE")
}

/// The caller's source address.
pub fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A caller's INVITE keyed by Call-ID, so each call gets a distinct
/// `callRef` (`{node}|{cid}@…|alicetag`).
#[expect(clippy::unwrap_used, clippy::panic, reason = "test support: a fixed INVITE parses")]
pub fn invite(cid: &str) -> SipRequest {
    // RFC 3261 §25.1: a Call-ID holds one `@`, a branch none.
    let call_id = if cid.contains('@') { cid.to_string() } else { format!("{cid}@10.0.0.9") };
    let branch = cid.replace('@', ".");
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{branch}\r\n\
         Record-Route: <sip:10.0.0.1:5060;lr>\r\n\
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
