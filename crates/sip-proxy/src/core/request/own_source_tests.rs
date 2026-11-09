//! A request whose datagram comes from one of this proxy's own addresses has
//! looped back to it: answered 482 Loop Detected on the face it arrived on (RFC
//! 3261 §16.3 item 4), an ACK dropped unanswered.

use std::net::SocketAddr;

use sip_message::SipMessage;

use crate::core::dual_face_tests::{
    dual_face_core, parse_msg, CALLEE, EXT_PORT, EXT_VIP, INT_PORT, INT_VIP, W1,
};
use crate::observability::metrics::{Face, RoutingDecisionKind};

/// A worker's INVITE as this proxy forwarded it from the face at `from`
/// toward a target naming its other face.
fn looped(method: &str, from: SocketAddr, to_tag: &str) -> SipMessage {
    let (host, port) = (from.ip(), from.port());
    parse_msg(&format!(
        "{method} sip:+100@{INT_VIP}:{INT_PORT};user=phone SIP/2.0\r\n\
Via: SIP/2.0/UDP {host}:{port};branch=z9hG4bKlooped;rport\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbleg1;rport=5060;received={W1}\r\n\
Max-Forwards: 67\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:+100@{CALLEE}>{to_tag}\r\n\
Call-ID: looped@test\r\n\
CSeq: 1 {method}\r\n\
Contact: <sip:b2bua@{W1}:5060;leg=b>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn ext_vip() -> SocketAddr {
    format!("{EXT_VIP}:{EXT_PORT}").parse().expect("an address")
}

fn int_vip() -> SocketAddr {
    format!("{INT_VIP}:{INT_PORT}").parse().expect("an address")
}

/// From the external face's address onto the internal face, and the other way
/// round: each is answered 482 from the face it arrived on, to its top Via, and
/// goes no further.
#[tokio::test]
async fn a_request_from_the_proxys_own_address_is_answered_482() {
    for (src, face, label) in
        [(ext_vip(), Face::Internal, "int"), (int_vip(), Face::External, "ext")]
    {
        let fx = dual_face_core();
        let outcome = fx.core.route_request(&looped("INVITE", src, ""), src, face).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::Reject, "from {src}");
        let sent = fx.take_sent();
        let [(on, dst, bytes)] = sent.as_slice() else { panic!("one answer: {sent:?}") };
        assert_eq!((*on, *dst), (label, src), "from {src}");
        let text = String::from_utf8_lossy(bytes);
        assert!(text.starts_with("SIP/2.0 482 Loop Detected\r\n"), "from {src}: {text}");
        assert_eq!(fx.metrics().reject_count("loop_detected"), 1, "from {src}");
    }
}

/// An ACK from the proxy's own address is dropped, answered by nothing.
#[tokio::test]
async fn an_ack_from_the_proxys_own_address_draws_nothing() {
    let fx = dual_face_core();
    let ack = looped("ACK", ext_vip(), ";tag=bobtag");
    fx.core.route_request(&ack, ext_vip(), Face::Internal).await;
    let sent = fx.take_sent();
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}
