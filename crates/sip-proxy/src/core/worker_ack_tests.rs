//! A worker's non-2xx ACK goes where its INVITE went (RFC 3261 §17.1.1.2),
//! whichever owner sends it. The owner that held the INVITE copies its Route; an
//! owner that holds no record of it states what its via-extension kept, which may
//! be no more than the plain Route at the proxy (`unheld_response`).

use std::net::SocketAddr;

use sip_message::SipMessage;

use super::dual_face_tests::{
    dual_face_core, header_lines, parse_msg, Fixture, CALLEE, INT_PORT, INT_VIP, W1,
};
use crate::addr::ProxyAddr;
use crate::observability::metrics::Face;

const HOP: &str = "192.168.60.30";

fn worker() -> SocketAddr {
    format!("{W1}:5060").parse().unwrap()
}

fn reinvite(call_id: &str) -> SipMessage {
    parse_msg(&format!(
        "INVITE sip:bob@{CALLEE}:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKreinv1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:b2bua@{W1}:5060;leg=b>\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{HOP}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn ack(call_id: &str, route: &str) -> SipMessage {
    parse_msg(&format!(
        "ACK sip:bob@{CALLEE}:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKreinv1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 2 ACK\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The re-INVITE leaves through the Route past the proxy, and its 488 comes back
/// from that hop through `f`.
async fn final_relayed(f: &Fixture, call_id: &str) {
    let out = f.core.route_request(&reinvite(call_id), worker(), Face::Internal).await;
    assert_eq!(out.target, Some(ProxyAddr::new(HOP, 5060)), "the re-INVITE's destination");
    let via = header_lines(&f.take_sent()[0].2, "Via");
    let ours = via[0].trim().to_string();
    let refused = parse_msg(&format!(
        "SIP/2.0 488 Not Acceptable Here\r\n\
Via: {ours}\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKreinv1;rport\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: 2 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let SipMessage::Response(refused) = refused else { panic!("a response") };
    let from_hop: SocketAddr = format!("{HOP}:5060").parse().unwrap();
    f.core.handle_response(refused, from_hop, Face::External).await;
    assert_eq!(f.take_sent().len(), 1, "the 488 relays to the worker");
}

fn plain_route() -> String {
    format!("<sip:{INT_VIP}:{INT_PORT};lr>")
}

/// §17.1.1.3: the holder copies the INVITE's Route.
#[tokio::test]
async fn an_ack_copying_the_invites_route_reaches_the_invites_hop() {
    let f = dual_face_core();
    final_relayed(&f, "held@test").await;
    let route = format!("<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{HOP}:5060;lr>");
    let out = f.core.route_request(&ack("held@test", &route), worker(), Face::Internal).await;
    assert_eq!(out.target, Some(ProxyAddr::new(HOP, 5060)));
}

/// The instance that forwarded the INVITE and relayed its final remembers where
/// the INVITE went: an ACK stating no route past the proxy still goes there.
#[tokio::test]
async fn an_ack_with_no_route_past_the_proxy_reaches_the_invites_hop_where_it_is_remembered() {
    let f = dual_face_core();
    final_relayed(&f, "unheld@test").await;
    let out =
        f.core.route_request(&ack("unheld@test", &plain_route()), worker(), Face::Internal).await;
    assert_eq!(out.target, Some(ProxyAddr::new(HOP, 5060)), "§17.1.1.2");
}

/// A Route that kept the hop places the ACK at an instance that saw neither the
/// INVITE nor its final.
#[tokio::test]
async fn an_ack_stating_the_hop_reaches_it_at_another_instance() {
    let other = dual_face_core();
    let route = format!("{}, <sip:{HOP}:5060;lr>", plain_route());
    let out = other.core.route_request(&ack("stated@test", &route), worker(), Face::Internal).await;
    assert_eq!(out.target, Some(ProxyAddr::new(HOP, 5060)));
}

/// The residual of ADR-0055 §12: a route set the via-extension's bound left out,
/// at an instance holding no memo, leaves the ACK its Request-URI alone.
#[tokio::test]
async fn an_ack_stating_no_hop_goes_to_its_request_uri_at_another_instance() {
    let other = dual_face_core();
    let out = other
        .core
        .route_request(&ack("unstated@test", &plain_route()), worker(), Face::Internal)
        .await;
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5070)));
}
