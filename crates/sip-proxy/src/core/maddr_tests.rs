//! A URI's `maddr` at the proxy: where a URI picks the next hop it is the target
//! (RFC 3261 §16.5, RFC 3263 §4), at the URI's own port; a Route naming the proxy
//! by it is the proxy's own; a Request-URI naming the proxy by it loses it
//! (§16.4), on either face. An inbound Request-URI on the external face chooses
//! nothing, and the proxy's sends keep a `maddr` it does not strip (§16.5).

use std::net::SocketAddr;

use sip_message::SipMessage;

use super::dual_face_tests::{
    dual_face_core, header_lines, parse_msg, Sent, CALLEE, CALLER, EXT_VIP, INT_PORT, INT_VIP, W1,
};
use crate::addr::ProxyAddr;
use crate::observability::metrics::RoutingDecisionKind;

const TARGET: &str = "192.0.2.9";

fn worker() -> SocketAddr {
    format!("{W1}:5060").parse().unwrap()
}

fn caller() -> SocketAddr {
    format!("{CALLER}:5060").parse().unwrap()
}

fn request_line(sent: &Sent) -> String {
    let text = String::from_utf8_lossy(&sent.2).to_string();
    text.lines().next().unwrap_or_default().to_string()
}

/// A worker's in-dialog BYE to `uri`, with `route` after the proxy's own entry.
fn worker_bye(uri: &str, route: &str) -> SipMessage {
    parse_msg(&format!(
        "BYE {uri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKbye1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{INT_VIP}:{INT_PORT}>;tag=svc\r\n\
To: <sip:bob@{CALLEE}>;tag=b-1\r\n\
Call-ID: maddr-bye@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};outbound;lr>{route}\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// An initial INVITE from the external caller to `uri`, its Route lines `route`.
fn caller_invite_to(uri: &str, route: &str) -> SipMessage {
    parse_msg(&format!(
        "INVITE {uri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER}:5060;branch=z9hG4bKcaller1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{CALLER}>;tag=tag-a\r\n\
To: <sip:bob@{CALLEE}>\r\n\
Call-ID: maddr-invite@test\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{CALLER}:5060>\r\n\
{route}Content-Length: 0\r\n\r\n"
    ))
}

/// A Contact-derived remote target: to the `maddr`, the name never read, the URI
/// kept whole as the Request-URI sent onward.
#[tokio::test]
async fn a_workers_request_goes_to_the_maddr_of_its_request_uri() {
    let f = dual_face_core();
    let uri = format!("sip:bob@name.invalid:5070;maddr={TARGET}");
    let out = f.core.route_request_from(&worker_bye(&uri, ""), worker()).await;
    assert_eq!(out.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(out.target, Some(ProxyAddr::new(TARGET, 5070)), "the URI's own port");
    let sent = f.take_sent();
    assert_eq!(sent[0].1, format!("{TARGET}:5070").parse::<SocketAddr>().unwrap());
    assert_eq!(request_line(&sent[0]), format!("BYE {uri} SIP/2.0"), "§16.5: `maddr` stays");
}

#[tokio::test]
async fn a_loose_route_past_the_proxys_own_goes_to_its_maddr() {
    let f = dual_face_core();
    let route = format!(", <sip:next.invalid;maddr={TARGET};lr>");
    let uri = format!("sip:bob@{CALLEE}:5060");
    let out = f.core.route_request_from(&worker_bye(&uri, &route), worker()).await;
    assert_eq!(out.decision, RoutingDecisionKind::LooseRoute);
    assert_eq!(out.target, Some(ProxyAddr::new(TARGET, 5060)), "the default port");
}

/// A Route naming the proxy only by its `maddr` is popped as the proxy's own, so
/// it cannot loop on Max-Forwards.
#[tokio::test]
async fn a_route_naming_the_proxy_by_its_maddr_is_the_proxys_own() {
    let f = dual_face_core();
    let route = format!(", <sip:name.invalid:{INT_PORT};maddr={INT_VIP};lr>");
    let uri = format!("sip:bob@{CALLEE}:5060");
    let out = f.core.route_request_from(&worker_bye(&uri, &route), worker()).await;
    assert_eq!(out.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5060)), "past the proxy's own entries");
    let sent = f.take_sent();
    assert!(header_lines(&sent[0].2, "Route").is_empty(), "the own entries are popped");
}

/// §16.4: `maddr` and a non-default port go, and the request goes on as if they
/// had not been there. The proxy's internal face listens on a non-default port.
#[tokio::test]
async fn a_request_uri_naming_the_proxy_by_its_maddr_loses_it_on_the_internal_face() {
    let f = dual_face_core();
    let uri = format!("sip:bob@{CALLEE}:{INT_PORT};transport=tcp;maddr={INT_VIP};user=phone");
    let out = f.core.route_request_from(&worker_bye(&uri, ""), worker()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5060)), "the URI as if never stated");
    let sent = f.take_sent();
    assert_eq!(request_line(&sent[0]), format!("BYE sip:bob@{CALLEE};user=phone SIP/2.0"));
}

#[tokio::test]
async fn a_request_uri_naming_the_proxy_by_its_maddr_loses_it_on_the_external_face() {
    let f = dual_face_core();
    let uri = format!("sip:bob@{CALLEE};maddr={EXT_VIP}");
    let out = f.core.route_request_from(&caller_invite_to(&uri, ""), caller()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(W1, 5060)));
    let sent = f.take_sent();
    assert_eq!(request_line(&sent[0]), format!("INVITE sip:bob@{CALLEE} SIP/2.0"));
}

/// The external face does not route on a Request-URI: its `maddr` steers nothing
/// there, and the worker reads it as sent.
#[tokio::test]
async fn an_inbound_request_uris_maddr_on_the_external_face_chooses_nothing() {
    let f = dual_face_core();
    let uri = format!("sip:bob@{CALLEE};maddr={TARGET}");
    let out = f.core.route_request_from(&caller_invite_to(&uri, ""), caller()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(W1, 5060)));
    let sent = f.take_sent();
    assert_eq!(request_line(&sent[0]), format!("INVITE {uri} SIP/2.0"));
}

/// The IPv4-mapped form names the same address: taken for another, each of these
/// would send the proxy to itself until Max-Forwards ran out.
#[tokio::test]
async fn a_route_naming_the_proxy_by_a_mapped_maddr_is_the_proxys_own() {
    let f = dual_face_core();
    let route = format!(", <sip:name.invalid:{INT_PORT};maddr=[::ffff:{INT_VIP}];lr>");
    let uri = format!("sip:bob@{CALLEE}:5060");
    let out = f.core.route_request_from(&worker_bye(&uri, &route), worker()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5060)));
    assert!(header_lines(&f.take_sent()[0].2, "Route").is_empty(), "the own entries are popped");
}

#[tokio::test]
async fn a_route_naming_the_proxy_by_a_mapped_authority_is_the_proxys_own() {
    let f = dual_face_core();
    let route = format!(", <sip:[::ffff:{INT_VIP}]:{INT_PORT};lr>");
    let uri = format!("sip:bob@{CALLEE}:5060");
    let out = f.core.route_request_from(&worker_bye(&uri, &route), worker()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5060)));
    assert!(header_lines(&f.take_sent()[0].2, "Route").is_empty(), "the own entries are popped");
}

#[tokio::test]
async fn a_request_uri_naming_the_proxy_by_a_mapped_maddr_loses_it() {
    let f = dual_face_core();
    let uri = format!("sip:bob@{CALLEE}:{INT_PORT};maddr=[::ffff:{INT_VIP}]");
    let out = f.core.route_request_from(&worker_bye(&uri, ""), worker()).await;
    assert_eq!(out.target, Some(ProxyAddr::new(CALLEE, 5060)));
    assert_eq!(request_line(&f.take_sent()[0]), format!("BYE sip:bob@{CALLEE} SIP/2.0"));
}
