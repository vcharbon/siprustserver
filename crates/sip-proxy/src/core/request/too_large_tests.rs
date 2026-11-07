//! A request that fits a datagram on arrival and not with this hop's Via and
//! Record-Route: the send refuses it, and the request is answered 513 Message
//! Too Large on the face it arrived on (RFC 3261 §21.5.14). An ACK draws
//! nothing, and neither does a send a resolve makes later, off the request's
//! path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_message::SipMessage;
use tokio::time::Instant;

use crate::core::dual_face_tests::{
    caller_invite, caller_src, dual_face_core, dual_face_core_over, parse_msg, worker_bleg_invite,
    CALLEE, INT_PORT, INT_VIP, W1,
};
use crate::observability::metrics::{Face, RoutingDecisionKind};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerEntry;
use crate::resolver::HostResolver;
use crate::ProxyAddr;

/// `msg` with a body that makes its datagram 8 bytes short of the largest UDP
/// payload.
fn filled(msg: &SipMessage) -> SipMessage {
    let text = String::from_utf8_lossy(msg.image()).to_string();
    let head = text.strip_suffix("Content-Length: 0\r\n\r\n").expect("an empty body");
    let frame = format!("{head}Content-Type: text/plain\r\nContent-Length: 00000\r\n\r\n");
    let length = sip_net::MAX_UDP_PAYLOAD - 8 - frame.len();
    let body = "b".repeat(length);
    let filled =
        format!("{head}Content-Type: text/plain\r\nContent-Length: {length}\r\n\r\n{body}");
    assert_eq!(filled.len(), sip_net::MAX_UDP_PAYLOAD - 8);
    parse_msg(&filled)
}

/// The caller's INVITE draws a 513 on the external face, and nothing reaches
/// the internal plane. Its retransmission draws the same 513 with no second
/// forward, and its CANCEL a 200 from the proxy, the INVITE's UAS (RFC 3261
/// §9.2).
#[tokio::test]
async fn an_invite_that_cannot_leave_with_the_hops_lines_is_answered_513_on_its_face() {
    let fx = dual_face_core();
    let invite = filled(&caller_invite("too-large"));
    let mut answers = Vec::new();
    for copy in ["first", "retransmission"] {
        let outcome = fx.core.route_request(&invite, caller_src(), Face::External).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::Reject, "{copy}");
        let sent = fx.take_sent();
        let [(face, dst, bytes)] = sent.as_slice() else { panic!("{copy}: one answer: {sent:?}") };
        assert_eq!((*face, *dst), ("ext", caller_src()), "{copy}");
        let text = String::from_utf8_lossy(bytes).to_string();
        assert!(text.starts_with("SIP/2.0 513 Message Too Large\r\n"), "{copy}: {text}");
        answers.push(text);
    }
    assert_eq!(answers[0], answers[1], "every copy draws the same 513");
    assert_eq!(fx.metrics().send_failures_total(), 1, "the copy is not forwarded again");
    assert_eq!(fx.metrics().reject_count("message_too_large"), 1);

    let cancel = String::from_utf8_lossy(caller_invite("too-large").image())
        .replacen("INVITE sip:", "CANCEL sip:", 1)
        .replacen("CSeq: 1 INVITE", "CSeq: 1 CANCEL", 1);
    fx.core.route_request(&parse_msg(&cancel), caller_src(), Face::External).await;
    let sent = fx.take_sent();
    let [(face, dst, bytes)] = sent.as_slice() else { panic!("one answer: {sent:?}") };
    assert_eq!((*face, *dst), ("ext", caller_src()));
    let text = String::from_utf8_lossy(bytes);
    assert!(text.starts_with("SIP/2.0 200 OK\r\n") && text.contains("CSeq: 1 CANCEL"), "{text}");
}

/// A worker's INVITE arriving on the internal face is answered there.
#[tokio::test]
async fn a_workers_invite_that_cannot_leave_is_answered_513_on_the_internal_face() {
    let fx = dual_face_core();
    let worker: SocketAddr = format!("{W1}:5060").parse().expect("an address");
    let invite = filled(&worker_bleg_invite("too-large-int"));
    fx.core.route_request(&invite, worker, Face::Internal).await;
    let sent = fx.take_sent();
    let [(face, dst, bytes)] = sent.as_slice() else { panic!("one answer: {sent:?}") };
    assert_eq!((*face, *dst), ("int", worker));
    assert!(bytes.starts_with(b"SIP/2.0 513 "), "{}", String::from_utf8_lossy(bytes));
}

/// Every name resolves to the callee.
struct CalleeResolver;

#[async_trait]
impl HostResolver for CalleeResolver {
    async fn resolve(&self, _host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        vec![SocketAddr::new(CALLEE.parse().expect("an address"), port)]
    }
}

/// A worker's INVITE to a name: the first copy is sent by the resolve it waits
/// on, off the request's path, and its refusal is counted only. Its
/// retransmission finds the name resolved, is sent inline, and is answered 513.
#[tokio::test(start_paused = true)]
async fn an_invite_to_a_name_is_answered_513_once_the_name_is_resolved() {
    let reg = Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
        "w1",
        ProxyAddr::new(W1, 5060),
    )]));
    let fx = dual_face_core_over(reg, Some(Arc::new(CalleeResolver)));
    let worker: SocketAddr = format!("{W1}:5060").parse().expect("an address");
    let named = String::from_utf8_lossy(worker_bleg_invite("too-large-named").image()).replacen(
        &format!("sip:bob@{CALLEE}:5060"),
        "sip:bob@callee.example:5060",
        1,
    );
    let invite = filled(&parse_msg(&named));
    fx.core.route_request(&invite, worker, Face::Internal).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fx.take_sent().is_empty(), "the resolved send is refused, unanswered");
    assert_eq!(fx.metrics().reject_count("message_too_large"), 0);

    fx.core.route_request(&invite, worker, Face::Internal).await;
    let sent = fx.take_sent();
    let [(face, dst, bytes)] = sent.as_slice() else { panic!("one answer: {sent:?}") };
    assert_eq!((*face, *dst), ("int", worker));
    assert!(bytes.starts_with(b"SIP/2.0 513 "), "{}", String::from_utf8_lossy(bytes));
}

/// An ACK of that size is dropped, answered by nothing.
#[tokio::test]
async fn an_ack_that_cannot_leave_draws_nothing() {
    let fx = dual_face_core();
    let worker: SocketAddr = format!("{W1}:5060").parse().expect("an address");
    let ack = parse_msg(&format!(
        "ACK sip:bob@{CALLEE}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKack2xx;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=bobtag\r\n\
Call-ID: too-large-ack\r\n\
CSeq: 1 ACK\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    fx.core.route_request(&filled(&ack), worker, Face::Internal).await;
    let sent = fx.take_sent();
    assert!(sent.is_empty(), "nothing is sent: {sent:?}");
}
