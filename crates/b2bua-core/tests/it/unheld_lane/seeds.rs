//! The responses the unheld-response lane mutates: finals to an INVITE
//! another node of the cluster sent and no node holds, on its Via signed
//! with the cluster key — a 2xx
//! routed by its Record-Route, a 2xx offering to an INVITE that carried
//! none, a non-2xx, a provisional — and a 2xx on a Via whose signature does
//! not verify. Addresses are documentation ranges (RFC 5737) and private
//! worker ones.

use b2bua_core::stack_identity::{build_call_via, RequestFacts, StackIdentityOpts};
use sip_message::header::HeaderValue;

use super::case::key;

/// One seed: a stable name and its bytes.
pub struct Seed {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

const OFFER: &str = "v=0\r\no=bob 7 7 IN IP4 192.0.2.30\r\ns=-\r\nc=IN IP4 192.0.2.30\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0 8\r\nm=video 20002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";

/// Another node's Via on the dialled INVITE of `b-1`, stating whether it
/// carried an offer, signed with the cluster key when `signed`.
fn node_via(offer: bool, signed: bool) -> String {
    let k = key();
    let facts =
        RequestFacts { method: "INVITE", uri: "sip:bob@192.0.2.30:5060;transport=udp", offer };
    let opts = StackIdentityOpts {
        local_ip: "10.0.0.8",
        local_port: 5060,
        call_ref: "w0|c1@alice|atag",
        leg: "b-1",
        incarnation_mark: "k3x9",
        is_emergency: false,
        other_owner: Some("w1"),
        request: signed.then_some((facts, &k)),
    };
    build_call_via(&opts, "z9hG4bKinvite".to_string()).to_wire()
}

fn response(status: u16, reason: &str, via: &str, sdp: &str) -> Vec<u8> {
    let typed = if sdp.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    format!(
        "SIP/2.0 {status} {reason}\r\n\
         Via: {via}\r\n\
         Record-Route: <sip:192.0.2.1:5060;lr>, <sip:10.0.0.1:5060;lr>\r\n\
         From: <sip:alice@example.com>;tag=b1tag\r\n\
         To: <sip:bob@example.net>;tag=bobtag\r\n\
         Call-ID: b-1-x7@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: \"Bob\" <sip:bob@192.0.2.30:5060;transport=udp>\r\n\
         {typed}Content-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    )
    .into_bytes()
}

/// Every seed.
pub fn all() -> Vec<Seed> {
    vec![
        Seed { name: "2xx", bytes: response(200, "OK", &node_via(true, true), OFFER) },
        Seed { name: "2xx-offer", bytes: response(200, "OK", &node_via(false, true), OFFER) },
        Seed { name: "non-2xx", bytes: response(486, "Busy Here", &node_via(true, true), "") },
        Seed { name: "1xx", bytes: response(183, "Progress", &node_via(true, true), OFFER) },
        Seed { name: "2xx-forged", bytes: response(200, "OK", &node_via(true, false), OFFER) },
    ]
}
