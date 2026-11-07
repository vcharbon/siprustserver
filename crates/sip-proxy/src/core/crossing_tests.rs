//! The dual-face egress table, a row per test (ADR-0048, ADR-0056): the face a
//! message leaves on, the advertise it states, read off the ingress face, the
//! message and the registry, never off an address plan. The addresses below
//! lie on purpose where an address plan would misread them: a caller on the
//! workers' prefix, a worker on another.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::SipMessage;
use tokio::time::Instant;

use super::dual_face_tests::{
    dual_face_core, dual_face_core_over, header_lines, parse_msg, Fixture, EXT_PORT, EXT_VIP,
    INT_PORT, INT_VIP, W1,
};
use crate::addr::ProxyAddr;
use crate::observability::metrics::{Direction, Face};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::resolver::HostResolver;

/// A registry worker outside the prefix the other workers share.
const W_ELSEWHERE: &str = "172.31.7.7";
/// A caller on the workers' prefix.
const NEAR_CALLER: &str = "10.244.9.9";
/// A host on the workers' prefix that is no worker.
const NEAR_HOST: &str = "10.244.7.7";
/// A sender on the internal face that is no registry worker.
const UNLISTED: &str = "10.244.9.99";
/// The node address a translated worker's datagrams arrive from.
const NODE: &str = "198.51.100.4";

fn sa(host: &str, port: u16) -> SocketAddr {
    format!("{host}:{port}").parse().unwrap()
}

fn two_workers() -> Arc<dyn WorkerRegistry> {
    Arc::new(StaticWorkerRegistry::from_entries(vec![
        WorkerEntry::alive("w1", ProxyAddr::new(W1, 5060)),
        WorkerEntry::alive("w2", ProxyAddr::new(W_ELSEWHERE, 5060)),
    ]))
}

fn core_over_two_workers() -> Fixture {
    dual_face_core_over(two_workers(), None)
}

fn int_advertise() -> String {
    format!("{INT_VIP}:{INT_PORT}")
}

fn ext_advertise() -> String {
    format!("{EXT_VIP}:{EXT_PORT}")
}

fn caller_invite(caller: &str) -> SipMessage {
    parse_msg(&format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP {caller}:5060;branch=z9hG4bKcaller;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing@{caller}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{caller}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// A BYE from `sender`, stating `route`, to the Request-URI `uri`.
fn bye(sender: &str, route: &str, uri: &str) -> SipMessage {
    parse_msg(&format!(
        "BYE {uri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {sender}:5060;branch=z9hG4bKbye;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-bye@example.com\r\n\
CSeq: 2 BYE\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn worker_marker() -> String {
    format!("<sip:{INT_VIP}:{INT_PORT};outbound;lr>")
}

/// The one datagram the last step sent, by the face that sent it.
fn one_sent(f: &Fixture) -> (&'static str, SocketAddr, Vec<u8>) {
    let mut sent = f.take_sent();
    assert_eq!(sent.len(), 1, "one datagram, got {}", sent.len());
    sent.remove(0)
}

fn top_via(bytes: &[u8]) -> String {
    header_lines(bytes, "Via").remove(0)
}

// ── Request, arrived external ───────────────────────────────────────────────

#[tokio::test]
async fn a_request_arriving_external_leaves_internal_whatever_its_source_address() {
    let f = dual_face_core();
    let src = sa(NEAR_CALLER, 5060);
    f.core.route_request(&caller_invite(NEAR_CALLER), src, Face::External).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W1, 5060)));
    assert!(top_via(&bytes).contains(&int_advertise()));
    // The caller's own later requests come back to the face she reached.
    let rrs = header_lines(&bytes, "Record-Route");
    assert!(rrs[0].contains(&int_advertise()) && rrs[0].contains(";outbound"), "{rrs:?}");
    assert!(rrs[1].contains(&ext_advertise()) && !rrs[1].contains(";outbound"), "{rrs:?}");
}

#[tokio::test]
async fn a_request_arriving_external_leaves_internal_whatever_its_target_address() {
    let f = core_over_two_workers();
    let route = format!("<sip:{EXT_VIP}:{EXT_PORT};target={W_ELSEWHERE}:5060;lr>");
    let msg = bye("192.168.60.10", &route, "sip:b2bua@example.com");
    f.core.route_request(&msg, sa("192.168.60.10", 5060), Face::External).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
    assert!(top_via(&bytes).contains(&int_advertise()));
}

// ── Request, arrived internal: worker-outbound or a loose hop ───────────────

/// A caller's Contact is hers to write: naming a host on the workers' prefix
/// aims the worker's request at nothing on the internal plane.
#[tokio::test]
async fn a_contact_naming_a_host_near_the_workers_leaves_external() {
    let f = core_over_two_workers();
    let uri = format!("sip:mallory@{NEAR_HOST}:9999");
    f.core.route_request(&bye(W1, &worker_marker(), &uri), sa(W1, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NEAR_HOST, 9999)));
    assert!(top_via(&bytes).contains(&ext_advertise()));
}

#[tokio::test]
async fn a_contact_naming_a_registry_worker_leaves_external() {
    let f = core_over_two_workers();
    let uri = format!("sip:mallory@{W_ELSEWHERE}:5060");
    f.core.route_request(&bye(W1, &worker_marker(), &uri), sa(W1, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(W_ELSEWHERE, 5060)));
    assert!(top_via(&bytes).contains(&ext_advertise()));
    let uri = format!("sip:mallory@{W1}:5060");
    f.core.route_request(&bye(W1, &worker_marker(), &uri), sa(W1, 5060), Face::Internal).await;
    assert_eq!(one_sent(&f).0, "ext", "the sender's own address too");
}

#[tokio::test]
async fn a_loose_hop_leaves_external_whatever_its_target() {
    let f = core_over_two_workers();
    let route =
        format!("<sip:{INT_VIP}:{INT_PORT};target={W1}:5060;lr>, <sip:{NEAR_HOST}:5060;lr>");
    let msg = bye(UNLISTED, &route, "sip:bob@example.com");
    f.core.route_request(&msg, sa(UNLISTED, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NEAR_HOST, 5060)));
    assert!(top_via(&bytes).contains(&ext_advertise()));
}

/// A worker's dialling INVITE leaves external, its Record-Route stating the
/// external face on top (the far side's) and the internal one below (the
/// worker's), its marker where it was.
#[tokio::test]
async fn a_workers_invite_to_a_near_host_records_both_faces() {
    let f = core_over_two_workers();
    let msg = parse_msg(&format!(
        "INVITE sip:bob@{NEAR_HOST}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKw;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing-dial@example.com\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{EXT_VIP}:{EXT_PORT}>\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    f.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    let (face, _, bytes) = one_sent(&f);
    assert_eq!(face, "ext");
    let rrs = header_lines(&bytes, "Record-Route");
    assert!(rrs[0].contains(&ext_advertise()) && !rrs[0].contains(";outbound"), "{rrs:?}");
    assert!(rrs[1].contains(&int_advertise()) && rrs[1].contains(";outbound"), "{rrs:?}");
}

// ── Request, arrived internal: neither ──────────────────────────────────────

/// The in-dialog request of a call that looped back through the platform with
/// no Record-Routing hop between its passes: two own pairs back to back, the
/// last pair naming the other call's worker. It re-enters and its response
/// comes back, both on the internal face.
#[tokio::test]
async fn a_re_entry_with_no_record_route_between_the_passes_stays_internal() {
    let f = core_over_two_workers();
    let route = format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>, \
         <sip:{EXT_VIP}:{EXT_PORT};target={W_ELSEWHERE}:5060;lr>, \
         <sip:{INT_VIP}:{INT_PORT};target={W_ELSEWHERE}:5060;outbound;lr>"
    );
    let msg = bye(W1, &route, &format!("sip:b2bua@{EXT_VIP}:{EXT_PORT}"));
    f.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
    assert!(top_via(&bytes).contains(&int_advertise()));

    let vias: String =
        header_lines(&bytes, "Via").iter().map(|v| format!("Via: {v}\r\n")).collect();
    let answer = parse_msg(&format!(
        "SIP/2.0 200 OK\r\n\
{vias}From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-bye@example.com\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let SipMessage::Response(resp) = answer else { panic!("a response") };
    f.core.handle_response(resp, sa(W_ELSEWHERE, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W1, 5060)));
}

/// A flow that moved reaches the other proxy, which holds no memo and picks the
/// same egress and the same sent-by from the message alone.
#[tokio::test]
async fn the_other_instance_picks_the_same_egress_with_no_memo() {
    let route = format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>, \
         <sip:{EXT_VIP}:{EXT_PORT};target={W_ELSEWHERE}:5060;lr>, \
         <sip:{INT_VIP}:{INT_PORT};target={W_ELSEWHERE}:5060;outbound;lr>"
    );
    let msg = bye(W1, &route, &format!("sip:b2bua@{EXT_VIP}:{EXT_PORT}"));
    let (one, other) = (core_over_two_workers(), core_over_two_workers());
    one.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    other.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    let (first, second) = (one_sent(&one), one_sent(&other));
    assert_eq!((first.0, first.1), ("int", sa(W_ELSEWHERE, 5060)));
    assert_eq!((second.0, second.1), (first.0, first.1));
    let via = |bytes: &[u8]| top_via(bytes).split(';').next().unwrap().to_string();
    assert_eq!(via(&first.2), via(&second.2), "one sent-by at either instance");
}

#[tokio::test]
async fn a_request_arriving_internal_for_any_other_target_leaves_external() {
    let f = core_over_two_workers();
    let route = format!("<sip:{INT_VIP}:{INT_PORT};target={NEAR_HOST}:5060;lr>");
    let msg = bye(UNLISTED, &route, "sip:bob@example.com");
    f.core.route_request(&msg, sa(UNLISTED, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NEAR_HOST, 5060)));
    assert!(top_via(&bytes).contains(&ext_advertise()));
}

// ── A name and the address it resolves to ───────────────────────────────────

struct NearResolver;

#[async_trait]
impl HostResolver for NearResolver {
    async fn resolve(&self, _host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        vec![sa(NEAR_HOST, port)]
    }
}

/// RFC 3261 §17.2.3 matches a request by its top Via's sent-by: the sent-by
/// stated and the socket used are one face's, whatever the name resolves to.
#[tokio::test(start_paused = true)]
async fn a_name_target_and_its_resolved_address_are_one_face() {
    let f = dual_face_core_over(two_workers(), Some(Arc::new(NearResolver)));
    let uri = "sip:bob@callee.example:5060";
    f.core.route_request(&bye(W1, &worker_marker(), uri), sa(W1, 5060), Face::Internal).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!(dst, sa(NEAR_HOST, 5060));
    assert!(top_via(&bytes).contains(&ext_advertise()));
    assert_eq!(face, "ext", "the socket of the sent-by the Via states");
}

// ── Responses ───────────────────────────────────────────────────────────────

fn response_with_vias(vias: &str) -> sip_message::types::SipResponse {
    let SipMessage::Response(resp) = parse_msg(&format!(
        "SIP/2.0 180 Ringing\r\n\
{vias}From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-resp@example.com\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        panic!("a response")
    };
    resp
}

#[tokio::test]
async fn a_response_arriving_external_leaves_internal_whatever_the_workers_address() {
    let f = core_over_two_workers();
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {EXT_VIP}:{EXT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {W_ELSEWHERE}:5060;branch=z9hG4bKw\r\n"
    ));
    f.core.handle_response(resp, sa("192.168.60.20", 5060), Face::External).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
}

#[tokio::test]
async fn a_response_the_owner_rule_sends_elsewhere_leaves_internal() {
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive("w1", ProxyAddr::new(W1, 5060))],
        Clock::test_at(0),
    );
    registry.set_address("w1", ProxyAddr::new(W_ELSEWHERE, 5060));
    let f = dual_face_core_over(Arc::new(registry), None);
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKw\r\n"
    ));
    f.core.handle_response(resp, sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
}

/// The next Via's sent-by names the worker: its `received` is a node address
/// no worker holds, and the response still stays on the internal plane.
#[tokio::test]
async fn a_response_to_a_translated_worker_leaves_internal() {
    let f = core_over_two_workers();
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKw;received={NODE};rport=40001\r\n"
    ));
    f.core.handle_response(resp, sa(W_ELSEWHERE, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(NODE, 40001)));
}

/// RFC 3581 §4: the response leaves from the address the request reached.
#[tokio::test]
async fn a_response_to_a_caller_leaves_external_whatever_her_address() {
    let f = core_over_two_workers();
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {NEAR_CALLER}:5060;branch=z9hG4bKc\r\n"
    ));
    f.core.handle_response(resp, sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NEAR_CALLER, 5060)));
    assert_eq!(f.metrics().face_messages_total(Face::External, Direction::Outbound), 1);
    assert_eq!(f.metrics().face_messages_total(Face::Internal, Direction::Outbound), 0);
}

// ── Own finals ──────────────────────────────────────────────────────────────

fn out_of_hops(sender: &str) -> SipMessage {
    parse_msg(&format!(
        "OPTIONS sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP {sender}:5060;branch=z9hG4bKhops;rport\r\n\
Max-Forwards: 0\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing-hops@example.com\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

#[tokio::test]
async fn an_own_final_leaves_the_external_face_it_was_asked_on() {
    let f = core_over_two_workers();
    let src = sa(NEAR_CALLER, 5060);
    f.core.route_request(&out_of_hops(NEAR_CALLER), src, Face::External).await;
    let (face, dst, bytes) = one_sent(&f);
    assert!(String::from_utf8_lossy(&bytes).starts_with("SIP/2.0 483"));
    assert_eq!((face, dst), ("ext", src));
}

#[tokio::test]
async fn an_own_final_leaves_the_internal_face_it_was_asked_on() {
    let f = core_over_two_workers();
    let src = sa(NODE, 40001);
    f.core.route_request(&out_of_hops(W1), src, Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert!(String::from_utf8_lossy(&bytes).starts_with("SIP/2.0 483"));
    assert_eq!((face, dst), ("int", src));
}

/// The residuals ADR-0048 states.
///
/// A sender no registry lists is no worker the response row can name.
#[tokio::test]
async fn a_response_for_a_sender_no_registry_holds_leaves_external() {
    let f = core_over_two_workers();
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {UNLISTED}:5060;branch=z9hG4bKu\r\n"
    ));
    f.core.handle_response(resp, sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(UNLISTED, 5060)));
}

/// A spiral's in-dialog INVITE refused by a translated worker: the final comes
/// from a node address and its ACK follows it there, off the registry.
#[tokio::test]
async fn a_spirals_ack_to_a_translated_worker_leaves_external() {
    let f = core_over_two_workers();
    let route = format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>, \
         <sip:{EXT_VIP}:{EXT_PORT};target={W_ELSEWHERE}:5060;lr>, \
         <sip:{INT_VIP}:{INT_PORT};target={W_ELSEWHERE}:5060;outbound;lr>"
    );
    let in_dialog = |method: &str, seq: &str| {
        parse_msg(&format!(
            "{method} sip:b2bua@{EXT_VIP}:{EXT_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKre;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-ack@example.com\r\n\
CSeq: {seq} {method}\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
        ))
    };
    f.core.route_request(&in_dialog("INVITE", "2"), sa(W1, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
    let vias: String =
        header_lines(&bytes, "Via").iter().map(|v| format!("Via: {v}\r\n")).collect();
    let SipMessage::Response(refused) = parse_msg(&format!(
        "SIP/2.0 488 Not Acceptable Here\r\n\
{vias}From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-ack@example.com\r\n\
CSeq: 2 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        panic!("a response")
    };
    f.core.handle_response(refused, sa(NODE, 40001), Face::Internal).await;
    assert_eq!(one_sent(&f).1, sa(W1, 5060), "the 488 relays to the sender");

    f.core.route_request(&in_dialog("ACK", "2"), sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NODE, 40001)));
}
