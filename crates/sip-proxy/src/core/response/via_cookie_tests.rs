//! The stickiness cookie on the Via this proxy pushes on a worker's INVITE
//! transaction, and the reverse-path failover that reads it back off a
//! response carrying no Record-Route (an in-dialog request's, or a 100).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerHealth};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::LoadBalancerStrategy;
use crate::strategy::{RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const UAS: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const CALL_ID: &str = "dlg-1@10.244.7.13";

/// Endpoint double keeping every datagram sent, with its destination.
#[derive(Default)]
struct Wire {
    sent: Mutex<Vec<(SocketAddr, Vec<u8>)>>,
}

impl Wire {
    fn take(&self) -> Vec<(SocketAddr, Vec<u8>)> {
        std::mem::take(&mut *self.sent.lock().unwrap())
    }
}

struct WireHandle(Arc<Wire>);

#[async_trait]
impl UdpEndpoint for WireHandle {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.0.sent.lock().unwrap().push((dst, buf.to_vec()));
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        format!("{PROXY_VIP}:5060").parse().unwrap()
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

struct Fixture {
    core: ProxyCore,
    strategy: Arc<LoadBalancerStrategy>,
    registry: SimulatedWorkerRegistry,
    wire: Arc<Wire>,
}

/// A proxy over the load balancer and two alive workers, `w1` and `w2`.
fn fixture() -> Fixture {
    let clock = Clock::test_at(0);
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060)),
            WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
        ],
        clock.clone(),
    );
    let metrics = Arc::new(ProxyMetrics::new());
    let strategy = Arc::new(LoadBalancerStrategy::new(
        Arc::new(registry.clone()),
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        metrics.clone(),
        clock.clone(),
    ));
    let wire = Arc::new(Wire::default());
    let core = ProxyCoreBuilder::for_test(
        ProxyAddr::new(PROXY_VIP, 5060),
        strategy.clone(),
        Arc::new(registry.clone()),
    )
    .clock(clock)
    .metrics(metrics)
    .build(Box::new(WireHandle(wire.clone())));
    Fixture { core, strategy, registry, wire }
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn w1() -> SocketAddr {
    format!("{W1_POD}:5060").parse().unwrap()
}

/// The dialog's cookie, as the proxy minted it for `w1`.
fn cookie(f: &Fixture) -> RouteParams {
    let invite = parse(&request("INVITE", "z9hG4bKmint", "", "", "1 INVITE"));
    f.strategy.encode_stickiness(&ProxyAddr::new(W1_POD, 5060), &invite).unwrap()
}

fn cookie_text(cookie: &RouteParams) -> String {
    cookie.iter().map(|(k, v)| format!(";{k}={v}")).collect()
}

/// A request `w1` sends toward the UAS through the proxy.
fn request(method: &str, branch: &str, route: &str, to_tag: &str, cseq: &str) -> String {
    format!(
        "{method} sip:bob@{UAS}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch={branch};rport\r\n\
{route}Max-Forwards: 70\r\n\
From: <sip:b2bua@{W1_POD}>;tag=w\r\n\
To: <sip:bob@{UAS}>{to_tag}\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: {cseq}\r\n\
Content-Length: 0\r\n\r\n"
    )
}

/// The dialog's route set as `w1` states it: the proxy's worker-facing
/// `;outbound` half, then its cookie half.
fn dialog_route(cookie: &RouteParams) -> String {
    format!(
        "Route: <sip:{PROXY_VIP}:5060;outbound;lr>, <sip:{PROXY_VIP}:5060{};lr>\r\n",
        cookie_text(cookie)
    )
}

fn top_via(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("via:"))
        .unwrap()
        .to_string()
}

/// The single datagram the last step put on the wire.
fn one_sent(f: &Fixture) -> (SocketAddr, Vec<u8>) {
    let mut sent = f.wire.take();
    assert_eq!(sent.len(), 1, "one datagram");
    sent.remove(0)
}

/// The UAS's `status` to the forwarded request `fwd`: its Via stack echoed,
/// no Record-Route (an in-dialog request's response, or a 100).
fn answer(fwd: &[u8], status: &str, to_tag: &str, cseq: &str) -> SipResponse {
    let vias: String = String::from_utf8_lossy(fwd)
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
        .map(|l| format!("{l}\r\n"))
        .collect();
    let SipMessage::Response(resp) = parse(&format!(
        "SIP/2.0 {status}\r\n\
{vias}From: <sip:b2bua@{W1_POD}>;tag=w\r\n\
To: <sip:bob@{UAS}>{to_tag}\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: {cseq}\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        unreachable!("a status line parses as a response")
    };
    resp
}

fn uas() -> SocketAddr {
    format!("{UAS}:5060").parse().unwrap()
}

// A worker's re-INVITE carries the dialog's cookie on the Via the proxy
// pushes, signed as the strategy signs it and without the Route's routing
// flags; its CANCEL carries the same Via (RFC 3261 §9.1).
#[tokio::test]
async fn a_workers_reinvite_and_its_cancel_carry_the_dialog_cookie_on_the_pushed_via() {
    let f = fixture();
    let cookie = cookie(&f);
    let route = dialog_route(&cookie);
    let reinvite = request("INVITE", "z9hG4bKre", &route, ";tag=b", "2 INVITE");
    f.core.handle_request_from(parse(&reinvite), w1()).await;
    let (_, fwd) = one_sent(&f);
    let via = top_via(&fwd);
    for (name, value) in &cookie {
        assert!(via.contains(&format!(";{name}={value}")), "{name} on the pushed Via: {via}");
    }
    assert!(!via.contains("outbound") && !via.contains(";lr"), "no routing flag: {via}");

    let cancel = request("CANCEL", "z9hG4bKre", &route, ";tag=b", "2 CANCEL");
    f.core.handle_request_from(parse(&cancel), w1()).await;
    let (_, cxl) = one_sent(&f);
    assert_eq!(top_via(&cxl), via, "the CANCEL's Via is its INVITE's");
}

// The ACK of a worker's re-INVITE's non-2xx final carries the re-INVITE's
// Via (RFC 3261 §17.1.1.3): the same route set, the same cookie.
#[tokio::test]
async fn a_workers_reinvite_and_its_non_2xx_ack_carry_one_via() {
    let f = fixture();
    let route = dialog_route(&cookie(&f));
    let reinvite = request("INVITE", "z9hG4bKre", &route, ";tag=b", "2 INVITE");
    f.core.handle_request_from(parse(&reinvite), w1()).await;
    let (_, fwd) = one_sent(&f);
    f.core
        .handle_response_from(answer(&fwd, "491 Request Pending", ";tag=b", "2 INVITE"), uas())
        .await;
    let _relayed = one_sent(&f);
    let ack = request("ACK", "z9hG4bKre", &route, ";tag=b", "2 ACK");
    f.core.handle_request_from(parse(&ack), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), top_via(&fwd), "the non-2xx ACK's Via is its INVITE's");
}

// A worker's initial INVITE states no cookie of its own (its preloaded Route
// carries none): its Via carries the cookie minted for the worker, the one its
// Record-Route carries, and so do its CANCEL (§9.1) and its non-2xx ACK
// (§17.1.1.3), which carry the same Route.
#[tokio::test]
async fn a_workers_initial_invite_and_its_cancel_and_ack_carry_the_minted_cookie() {
    let f = fixture();
    let minted = cookie(&f);
    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let invite = request("INVITE", "z9hG4bKini", &route, "", "1 INVITE");
    f.core.handle_request_from(parse(&invite), w1()).await;
    let (_, fwd) = one_sent(&f);
    let via = top_via(&fwd);
    for (name, value) in &minted {
        assert!(via.contains(&format!(";{name}={value}")), "{name} on the pushed Via: {via}");
    }
    let cancel = request("CANCEL", "z9hG4bKini", &route, "", "1 CANCEL");
    f.core.handle_request_from(parse(&cancel), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), via, "the CANCEL's Via is its INVITE's");
    let refused = answer(&fwd, "487 Request Terminated", ";tag=b", "1 INVITE");
    f.core.handle_response_from(refused, uas()).await;
    let _relayed = one_sent(&f);
    let ack = request("ACK", "z9hG4bKini", &route, ";tag=b", "1 ACK");
    f.core.handle_request_from(parse(&ack), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), via, "the non-2xx ACK's Via is its INVITE's");
}

/// `w1`'s re-INVITE forwarded, then `w1` dead: the forwarded bytes.
async fn reinvite_forwarded_then_w1_dead(f: &Fixture) -> Vec<u8> {
    let route = dialog_route(&cookie(f));
    let reinvite = request("INVITE", "z9hG4bKre", &route, ";tag=b", "2 INVITE");
    f.core.handle_request_from(parse(&reinvite), w1()).await;
    let (_, fwd) = one_sent(f);
    f.registry.set_health("w1", WorkerHealth::Dead);
    fwd
}

// An INVITE response with no Record-Route, bound for a dead worker, fails over
// to the backup on the cookie of the proxy's own Via: the 2xx and the 100
// alike.
#[tokio::test]
async fn a_response_without_record_route_fails_over_on_the_via_cookie() {
    let f = fixture();
    let fwd = reinvite_forwarded_then_w1_dead(&f).await;
    for status in ["100 Trying", "200 OK"] {
        f.core.handle_response_from(answer(&fwd, status, ";tag=b", "2 INVITE"), uas()).await;
        let (dst, _) = one_sent(&f);
        assert_eq!(dst, format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap(), "{status}");
    }
}

// A Via cookie whose signature does not verify is not honoured: the response
// bound for the dead worker is dropped, never failed over.
#[tokio::test]
async fn a_tampered_via_cookie_is_not_honoured() {
    let f = fixture();
    let fwd = reinvite_forwarded_then_w1_dead(&f).await;
    let text = String::from_utf8_lossy(&fwd).to_string();
    let sig = text.split(";sig=").nth(1).unwrap().split([';', '\r']).next().unwrap();
    let tampered = text.replacen(&format!(";sig={sig}"), ";sig=AAAAAAAAAAAAAAAAAAAAAA", 1);
    f.core
        .handle_response_from(answer(tampered.as_bytes(), "200 OK", ";tag=b", "2 INVITE"), uas())
        .await;
    assert!(f.wire.take().is_empty(), "a forged cookie routes nothing");
}

// A param on the proxy's Route entry that is not one of the cookie's fields —
// a downstream party's echo of the Record-Route, say — never reaches the Via,
// and never replaces a param the proxy set there itself: the §16.11 branch
// stays the proxy's, and no `maddr` steers the response (§18.2.2).
#[tokio::test]
async fn only_the_cookie_fields_reach_the_via() {
    let f = fixture();
    let cookie = cookie(&f);
    let route = format!(
        "Route: <sip:{PROXY_VIP}:5060;outbound;lr>, \
         <sip:{PROXY_VIP}:5060{};maddr=192.0.2.9;branch=z9hG4bKforced;rport=9;lr>\r\n",
        cookie_text(&cookie)
    );
    let reinvite = request("INVITE", "z9hG4bKre", &route, ";tag=b", "2 INVITE");
    f.core.handle_request_from(parse(&reinvite), w1()).await;
    let via = top_via(&one_sent(&f).1);
    assert!(!via.contains("maddr") && !via.contains("rport=9"), "{via}");
    assert!(!via.contains("z9hG4bKforced"), "the §16.11 branch stays the proxy's: {via}");
    assert!(via.contains(&format!(";sig={}", cookie["sig"])), "the cookie rides on: {via}");
}

// A cookie on the route set whose signature does not verify for the call is
// never carried onto the proxy's Via: the Via carries the cookie minted for
// the sending worker in its place.
#[tokio::test]
async fn a_forged_route_cookie_is_not_carried() {
    let f = fixture();
    let minted = cookie(&f);
    let mut forged = minted.clone();
    forged.insert("w_bak".into(), "w9".into());
    let reinvite = request("INVITE", "z9hG4bKre", &dialog_route(&forged), ";tag=b", "2 INVITE");
    f.core.handle_request_from(parse(&reinvite), w1()).await;
    let via = top_via(&one_sent(&f).1);
    assert!(!via.contains("w_bak=w9"), "{via}");
    assert!(via.contains(&format!(";sig={}", minted["sig"])), "the minted cookie rides: {via}");
}

/// `raw` with a `Resource-Priority` emergency value after its Via.
fn emergency(raw: &str) -> String {
    raw.replacen("rport\r\n", "rport\r\nResource-Priority: esnet.0\r\n", 1)
}

/// The proxy's Record-Route line that carries the cookie, if any.
fn cookie_record_route(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("record-route:") && l.contains("sig="))
        .map(str::to_string)
}

/// `w1`'s initial INVITE forwarded: its bytes. `raw` is the INVITE.
async fn initial_forwarded(f: &Fixture, raw: &str) -> Vec<u8> {
    f.core.handle_request_from(parse(raw), w1()).await;
    one_sent(f).1
}

fn preloaded() -> String {
    format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n")
}

// An emergency INVITE's cookie says so (`e=1`); its CANCEL carries no
// Resource-Priority, and still carries the INVITE's Via (§9.1).
#[tokio::test]
async fn an_emergency_invites_cancel_carries_its_via() {
    let f = fixture();
    let invite = emergency(&request("INVITE", "z9hG4bKem", &preloaded(), "", "1 INVITE"));
    let via = top_via(&initial_forwarded(&f, &invite).await);
    assert!(via.contains(";e=1"), "{via}");
    let cancel = request("CANCEL", "z9hG4bKem", &preloaded(), "", "1 CANCEL");
    f.core.handle_request_from(parse(&cancel), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), via, "the CANCEL's Via is its INVITE's");
}

// The worker set moves between an INVITE and its CANCEL: the CANCEL still
// carries the INVITE's Via, whose backup the set no longer offers.
#[tokio::test]
async fn a_cancel_after_the_worker_set_moved_carries_its_invites_via() {
    let f = fixture();
    let invite = request("INVITE", "z9hG4bKmv", &preloaded(), "", "1 INVITE");
    let via = top_via(&initial_forwarded(&f, &invite).await);
    f.registry.remove("w2");
    let cancel = request("CANCEL", "z9hG4bKmv", &preloaded(), "", "1 CANCEL");
    f.core.handle_request_from(parse(&cancel), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), via, "the CANCEL's Via is its INVITE's");
}

// The INVITE's worker departs and its address's window lapses on a long ring:
// the CANCEL it still sends carries the INVITE's Via.
#[tokio::test(start_paused = true)]
async fn a_cancel_past_its_workers_tombstone_carries_its_invites_via() {
    let f = fixture();
    f.registry.remove("w1");
    let invite = request("INVITE", "z9hG4bKlap", &preloaded(), "", "1 INVITE");
    let via = top_via(&initial_forwarded(&f, &invite).await);
    let window = crate::registry::tombstone::DEPARTED_ADDRESS_TTL_MS;
    tokio::time::advance(std::time::Duration::from_millis(window + 1_000)).await;
    let cancel = request("CANCEL", "z9hG4bKlap", &preloaded(), "", "1 CANCEL");
    f.core.handle_request_from(parse(&cancel), w1()).await;
    assert_eq!(top_via(&one_sent(&f).1), via, "the CANCEL's Via is its INVITE's");
}

// A retransmission of a worker's INVITE repeats the cookie its first copy
// carried, on the Record-Route and on the Via, whatever the set did since.
#[tokio::test]
async fn a_retransmitted_invite_repeats_its_first_copys_cookies() {
    let f = fixture();
    let invite = request("INVITE", "z9hG4bKrtx", &preloaded(), "", "1 INVITE");
    let first = initial_forwarded(&f, &invite).await;
    f.registry.remove("w2");
    let again = initial_forwarded(&f, &invite).await;
    assert_eq!(top_via(&again), top_via(&first), "the Via cookie repeats");
    assert_eq!(cookie_record_route(&again), cookie_record_route(&first), "so does the RR's");
    assert!(cookie_record_route(&first).is_some_and(|rr| rr.contains("w_bak=w2")));
}
