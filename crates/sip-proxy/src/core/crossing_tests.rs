//! The dual-face egress table, a row per test (ADR-0048, ADR-0056): the face a
//! message leaves on, the advertise it states, read off the ingress face, the
//! message and the registry, never off an address plan. The addresses below
//! lie on purpose where an address plan would misread them: a caller on the
//! workers' prefix, a worker on another.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::SipMessage;
use tokio::time::Instant;

use super::dual_face_tests::{
    dual_face_core, dual_face_core_over, dual_face_core_routing, header_lines, parse_msg, Fixture,
    EXT_PORT, EXT_VIP, INT_PORT, INT_VIP, W1,
};
use crate::addr::ProxyAddr;
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::observability::metrics::{Direction, Face};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::resolver::HostResolver;
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::strategies::LoadBalancerStrategy;
use crate::strategy::{RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

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

/// A dual-face core over the two workers and the load balancer, whose cookies
/// verify under one key at every instance, with that strategy.
fn signed_core() -> (Fixture, Arc<LoadBalancerStrategy>) {
    let clock = Clock::test_at(0);
    let registry = two_workers();
    let strategy = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Arc::new(ProxyMetrics::new()),
        clock,
    ));
    (dual_face_core_routing(registry, strategy.clone(), None), strategy)
}

/// `params` as the fields of a Route URI.
fn fields(params: &RouteParams) -> String {
    let field = |(k, v): (&String, &String)| format!(";{k}={v}");
    params.iter().map(field).collect()
}

/// The route set of W1 toward W_ELSEWHERE on `msg`'s call, which looped back
/// through the platform with no Record-Routing hop between its passes: two of
/// this proxy's pairs back to back, each cookie signed for the call.
fn re_entry_route(strategy: &LoadBalancerStrategy, msg: &SipMessage) -> String {
    let cookie = |worker: &str| {
        fields(&strategy.encode_stickiness(&ProxyAddr::new(worker, 5060), msg).expect("a cookie"))
    };
    let (first, last) = (cookie(W1), cookie(W_ELSEWHERE));
    format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT}{first};lr>, \
         <sip:{EXT_VIP}:{EXT_PORT}{last};lr>, <sip:{INT_VIP}:{INT_PORT}{last};outbound;lr>"
    )
}

/// A BYE of the call `re_entry_route` states, from W1.
fn re_entry_bye(strategy: &LoadBalancerStrategy) -> SipMessage {
    let call = bye(W1, &worker_marker(), "sip:b2bua@example.com");
    let route = re_entry_route(strategy, &call);
    bye(W1, &route, &format!("sip:b2bua@{EXT_VIP}:{EXT_PORT}"))
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
    let (f, strategy) = signed_core();
    f.core.route_request(&re_entry_bye(&strategy), sa(W1, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
    assert!(top_via(&bytes).contains(&int_advertise()));
    assert!(top_via(&bytes).contains(";same-face"), "{}", top_via(&bytes));

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
    let (face, dst, relayed) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W1, 5060)));
    assert!(
        !String::from_utf8_lossy(&relayed).contains("same-face"),
        "the marker rides the proxy's own Via, popped before the response leaves"
    );
}

/// A flow that moved reaches the other proxy, which holds no memo and picks the
/// same egress and the same sent-by from the message alone.
#[tokio::test]
async fn the_other_instance_picks_the_same_egress_with_no_memo() {
    let (one, strategy) = signed_core();
    let (other, _) = signed_core();
    let msg = re_entry_bye(&strategy);
    one.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    other.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    let (first, second) = (one_sent(&one), one_sent(&other));
    assert_eq!((first.0, first.1), ("int", sa(W_ELSEWHERE, 5060)));
    assert_eq!((second.0, second.1), (first.0, first.1));
    let via = |bytes: &[u8]| top_via(bytes).split(';').next().unwrap().to_string();
    assert_eq!(via(&first.2), via(&second.2), "one sent-by at either instance");

    // The response crosses back through the instance that did not forward it.
    let vias: String =
        header_lines(&first.2, "Via").iter().map(|v| format!("Via: {v}\r\n")).collect();
    let SipMessage::Response(resp) = parse_msg(&format!(
        "SIP/2.0 200 OK\r\n\
{vias}From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-bye@example.com\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        panic!("a response")
    };
    other.core.handle_response(resp, sa(W_ELSEWHERE, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&other);
    assert_eq!((face, dst), ("int", sa(W1, 5060)), "the marker holds at either instance");
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

struct CountingResolver(Arc<AtomicU32>);

#[async_trait]
impl HostResolver for CountingResolver {
    async fn resolve(&self, _host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        self.0.fetch_add(1, Ordering::SeqCst);
        vec![sa(NEAR_HOST, port)]
    }
}

/// A prewarmed name is a worker's dialled target, so it warms the face that
/// request leaves on: the first send finds it, with no second lookup.
#[tokio::test(start_paused = true)]
async fn a_prewarmed_name_is_warm_on_the_face_its_request_leaves() {
    let lookups = Arc::new(AtomicU32::new(0));
    let resolver = Arc::new(CountingResolver(lookups.clone()));
    let f = dual_face_core_over(two_workers(), Some(resolver));
    f.core.prewarm_named_targets(vec![ProxyAddr::new("callee.example", 5060)]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(lookups.load(Ordering::SeqCst), 1, "the prewarm resolved it");

    let uri = "sip:bob@callee.example:5060";
    f.core.route_request(&bye(W1, &worker_marker(), uri), sa(W1, 5060), Face::Internal).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa(NEAR_HOST, 5060)));
    assert_eq!(lookups.load(Ordering::SeqCst), 1, "no second lookup");
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
    let f = dual_face_core_over(w1_moved(), None);
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp;same-face\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKw\r\n"
    ));
    f.core.handle_response(resp, sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W_ELSEWHERE, 5060)));
}

/// The pool after `w1` moved from `W1` to `W_ELSEWHERE`: the address it left
/// reads as a departed worker, `Dead`, for its window.
fn w1_moved() -> Arc<dyn WorkerRegistry> {
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive("w1", ProxyAddr::new(W1, 5060))],
        Clock::test_at(0),
    );
    registry.set_address("w1", ProxyAddr::new(W_ELSEWHERE, 5060));
    Arc::new(registry)
}

/// A callee behind a NAT whose private sent-by equals an address a worker left
/// answers a request that left external: her response arrives internal with no
/// marker, so it belongs to no worker, whatever the registry says of that
/// address, and goes back to her on the external face.
#[tokio::test]
async fn an_unmarked_response_naming_a_departed_worker_follows_its_via_external() {
    let f = dual_face_core_over(w1_moved(), None);
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKc;received=203.0.113.66;rport=40000\r\n"
    ));
    f.core.handle_response(resp, sa(W_ELSEWHERE, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa("203.0.113.66", 40000)), "to her, not to w1's new address");
}

/// The same with the address read `Dead` and nowhere to move: not dropped.
#[tokio::test]
async fn an_unmarked_response_naming_a_dead_worker_is_not_dropped() {
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive("w1", ProxyAddr::new(W1, 5060))],
        Clock::test_at(0),
    );
    registry.set_health("w1", crate::registry::WorkerHealth::Dead);
    let f = dual_face_core_over(Arc::new(registry), None);
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKc;received=203.0.113.66;rport=40000\r\n"
    ));
    f.core.handle_response(resp, sa(W_ELSEWHERE, 5060), Face::Internal).await;
    let sent = f.take_sent();
    assert_eq!(sent.len(), 1, "not dropped");
    assert_eq!((sent[0].0, sent[0].1), ("ext", sa("203.0.113.66", 40000)));
}

/// The owner rule's own answer for a worker holding its transaction, along its
/// Via: the cookie this proxy put on its top Via names that worker.
#[tokio::test]
async fn a_response_on_a_cookie_for_the_worker_holding_it_leaves_internal() {
    let (f, _) = signed_core();
    let dial = parse_msg(&format!(
        "INVITE sip:bob@{NEAR_HOST}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKw;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing-held@example.com\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{EXT_VIP}:{EXT_PORT}>\r\n\
Route: <sip:{INT_VIP}:{INT_PORT};lr>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    f.core.route_request(&dial, sa(W1, 5060), Face::Internal).await;
    let (face, _, bytes) = one_sent(&f);
    assert_eq!(face, "ext");
    let via = header_lines(&bytes, "Via");
    assert!(via[0].contains("w_pri="), "the proxy's Via carries the cookie: {}", via[0]);
    let vias: String = via.iter().map(|v| format!("Via: {v}\r\n")).collect();
    let SipMessage::Response(resp) = parse_msg(&format!(
        "SIP/2.0 180 Ringing\r\n\
{vias}From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-held@example.com\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        panic!("a response")
    };
    f.core.handle_response(resp, sa(NEAR_HOST, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W1, 5060)));
}

/// The marker on the proxy's own Via keeps the response internal: the next Via's
/// `received` is a node address no worker holds.
#[tokio::test]
async fn a_marked_response_to_a_translated_worker_leaves_internal() {
    let f = core_over_two_workers();
    let resp = response_with_vias(&format!(
        "Via: SIP/2.0/UDP {INT_VIP}:{INT_PORT};branch=z9hG4bKp;same-face\r\n\
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

// ── The residuals ADR-0048 states ───────────────────────────────────────────

/// A sender no registry lists, whose request stayed internal: the marker brings
/// its response back the way it came.
#[tokio::test]
async fn a_sender_no_registry_holds_gets_its_response_where_its_request_stayed() {
    let f = core_over_two_workers();
    let route = format!("<sip:{INT_VIP}:{INT_PORT};target={W1}:5060;lr>");
    let msg = bye(UNLISTED, &route, "sip:bob@example.com");
    f.core.route_request(&msg, sa(UNLISTED, 5060), Face::Internal).await;
    let (face, dst, bytes) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(W1, 5060)));
    let vias: String =
        header_lines(&bytes, "Via").iter().map(|v| format!("Via: {v}\r\n")).collect();
    f.core.handle_response(response_with_vias(&vias), sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("int", sa(UNLISTED, 5060)));
}

/// A spiral's in-dialog INVITE refused by a translated worker: the final comes
/// from a node address and its ACK follows it there, on the internal face its
/// INVITE took: the ACK re-enters on the same verified cookie.
#[tokio::test]
async fn a_spirals_ack_to_a_translated_worker_stays_internal() {
    let (f, strategy) = signed_core();
    let of_the_call = parse_msg(
        "OPTIONS sip:b2bua@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.5.8:5060;branch=z9hG4bKcall\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing-ack@example.com\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n",
    );
    let route = re_entry_route(&strategy, &of_the_call);
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
    assert_eq!((face, dst), ("int", sa(NODE, 40001)));
}

/// A caller's Record-Route below the proxy's pair, two entries naming the proxy
/// and no marker: the worker's route set toward her holds four own entries and
/// no recorded pass. Its keepalive or BYE goes to her, on the external face.
#[tokio::test]
async fn a_callers_record_route_does_not_aim_a_worker_at_the_internal_plane() {
    let f = core_over_two_workers();
    let route = format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};lr>, \
         <sip:{EXT_VIP}:{EXT_PORT};lr>, <sip:{INT_VIP}:{INT_PORT};lr>"
    );
    let msg = bye(W1, &route, "sip:mallory@203.0.113.66:5060");
    f.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
    let (face, dst, _) = one_sent(&f);
    assert_eq!((face, dst), ("ext", sa("203.0.113.66", 5060)), "to her, on the external face");
}

/// The same over the load balancer, a cookie signed for another call among them:
/// only one that verifies for this call is a recorded pass. No fresh selection
/// runs and no worker hears it.
#[tokio::test]
async fn a_cookie_that_does_not_verify_for_the_call_proves_no_pass() {
    let (f, strategy) = signed_core();
    let elsewhere = parse_msg(
        "OPTIONS sip:b2bua@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.244.5.8:5060;branch=z9hG4bKother\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: another-call@example.com\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n",
    );
    let replayed = fields(
        &strategy
            .encode_stickiness(&ProxyAddr::new(W_ELSEWHERE, 5060), &elsewhere)
            .expect("cookie"),
    );
    for last in ["", replayed.as_str()] {
        let route = format!(
            "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};lr>, \
             <sip:{EXT_VIP}:{EXT_PORT}{last};lr>, <sip:{INT_VIP}:{INT_PORT};lr>"
        );
        let msg = bye(W1, &route, "sip:mallory@203.0.113.66:5060");
        f.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
        let (face, dst, _) = one_sent(&f);
        assert_eq!((face, dst), ("ext", sa("203.0.113.66", 5060)), "last entry: {last:?}");
    }
}

/// A phone behind a NAT whose private sent-by happens to equal a worker's
/// address: her replies leave the face her INVITE reached, whatever that Via
/// names. Nothing she wrote keeps a response on the internal plane, a marker on
/// her own Via included.
#[tokio::test]
async fn a_caller_whose_via_names_a_worker_is_answered_from_the_external_face() {
    for own_via in ["", ";same-face"] {
        let f = core_over_two_workers();
        let invite = parse_msg(&format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP {W_ELSEWHERE}:5060;branch=z9hG4bKnat;rport{own_via}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: crossing-nat@example.com\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@203.0.113.66:40000>\r\n\
Content-Length: 0\r\n\r\n"
        ));
        let src = sa("203.0.113.66", 40000);
        f.core.route_request(&invite, src, Face::External).await;
        let (_, _, bytes) = one_sent(&f);
        let vias: String =
            header_lines(&bytes, "Via").iter().map(|v| format!("Via: {v}\r\n")).collect();
        f.core.handle_response(response_with_vias(&vias), sa(W1, 5060), Face::Internal).await;
        let (face, dst, _) = one_sent(&f);
        assert_eq!((face, dst), ("ext", src), "the face her INVITE reached, her Via {own_via:?}");
    }
}

/// The load balancer's decisions over a pool that no longer lists the worker a
/// cookie names: the cookie still verifies, and still places the request on the
/// address it states.
struct PlacedOnTheCookie(ForwardAllStrategy);

#[async_trait]
impl RoutingStrategy for PlacedOnTheCookie {
    fn name(&self) -> &str {
        "PlacedOnTheCookie"
    }
    async fn select_for_new_dialog(
        &self,
        msg: &SipMessage,
        opts: crate::strategy::SelectOpts,
    ) -> Result<ProxyAddr, crate::strategy::SelectError> {
        self.0.select_for_new_dialog(msg, opts).await
    }
    async fn decode_stickiness(
        &self,
        params: &RouteParams,
        msg: &SipMessage,
    ) -> crate::strategy::DecodeResult {
        self.0.decode_stickiness(params, msg).await
    }
    fn encode_stickiness(&self, target: &ProxyAddr, msg: &SipMessage) -> Option<RouteParams> {
        self.0.encode_stickiness(target, msg)
    }
    fn stickiness_primary<'p>(&self, params: &'p RouteParams) -> Option<&'p str> {
        self.0.stickiness_primary(params)
    }
    fn verified_cookie(&self, params: &RouteParams, _msg: &SipMessage) -> Option<RouteParams> {
        params.get("target").map(|t| RouteParams::from([("target".to_string(), t.clone())]))
    }
}

/// A re-entry's INVITE and its CANCEL take one face whatever the registry did
/// between them: a request on a recorded pass is bound for the worker its
/// cookie names, so its Via states one sent-by (RFC 3261 §9.1, §17.2.3).
#[tokio::test]
async fn a_re_entry_and_its_cancel_stay_internal_past_the_registry() {
    let left = "10.244.5.77";
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(PlacedOnTheCookie(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060))));
    let f = dual_face_core_routing(two_workers(), strategy, None);
    let route = format!(
        "<sip:{INT_VIP}:{INT_PORT};outbound;lr>, <sip:{EXT_VIP}:{EXT_PORT};target={W1}:5060;lr>, \
         <sip:{EXT_VIP}:{EXT_PORT};target={left}:5060;lr>, \
         <sip:{INT_VIP}:{INT_PORT};target={left}:5060;outbound;lr>"
    );
    for method in ["INVITE", "CANCEL"] {
        let msg = parse_msg(&format!(
            "{method} sip:b2bua@{EXT_VIP}:{EXT_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1}:5060;branch=z9hG4bKre-cancel;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@example.com>;tag=w\r\n\
To: <sip:bob@example.com>;tag=b\r\n\
Call-ID: crossing-re-cancel@example.com\r\n\
CSeq: 2 {method}\r\n\
Route: {route}\r\n\
Content-Length: 0\r\n\r\n"
        ));
        f.core.route_request(&msg, sa(W1, 5060), Face::Internal).await;
        let (face, dst, bytes) = one_sent(&f);
        assert_eq!((face, dst), ("int", sa(left, 5060)), "{method}");
        assert!(top_via(&bytes).contains(&int_advertise()), "{method}: {}", top_via(&bytes));
    }
}
