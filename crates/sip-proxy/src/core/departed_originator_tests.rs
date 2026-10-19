//! A dialog-creating request from a worker whose address the proxy has just
//! departed: the stickiness cookie names the departed ordinal and an alive
//! backup, so the dialog's INVITE responses fail over to that backup.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::header::RecordRouteEntry;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::headers::cookie_params;
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::tombstone::DEPARTED_ADDRESS_TTL_MS;
use crate::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::LoadBalancerStrategy;
use crate::strategy::{RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const STRANGER: &str = "10.244.6.66";
const REPLACEMENT: &str = "10.244.5.10";
const UAS: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const CALL_ID: &str = "departed-1@10.244.5.8";

/// Endpoint double keeping every datagram sent, with its destination.
#[derive(Default)]
struct Wire {
    sent: Mutex<Vec<(SocketAddr, Vec<u8>)>>,
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
    metrics: Arc<ProxyMetrics>,
    wire: Arc<Wire>,
}

impl Fixture {
    /// The single datagram the last step put on the wire.
    fn one_sent(&self) -> (SocketAddr, Vec<u8>) {
        let mut sent = std::mem::take(&mut *self.wire.sent.lock().unwrap());
        assert_eq!(sent.len(), 1, "one datagram");
        sent.remove(0)
    }
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
    .metrics(metrics.clone())
    .build(Box::new(WireHandle(wire.clone())));
    Fixture { core, strategy, registry, metrics, wire }
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

/// The initial INVITE the worker at `pod` sends toward the UAS through the
/// proxy.
fn initial_invite(pod: &str) -> String {
    format!(
        "INVITE sip:bob@{UAS}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {pod}:5060;branch=z9hG4bKdeparted;rport\r\n\
Route: <sip:{PROXY_VIP}:5060;lr>\r\n\
Max-Forwards: 70\r\n\
From: <sip:b2bua@{pod}>;tag=w\r\n\
To: <sip:bob@{UAS}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{pod}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    )
}

/// The proxy's own cookie on the Record-Route of the forwarded request.
fn recorded_cookie(fwd: &[u8]) -> RouteParams {
    let SipMessage::Request(req) = parse(&String::from_utf8_lossy(fwd)) else {
        unreachable!("the proxy forwarded a request")
    };
    let entries = req.list::<RecordRouteEntry>().unwrap_or_default();
    entries
        .iter()
        .map(|entry| cookie_params(entry.uri()))
        .find(|params| params.contains_key("sig"))
        .expect("a Record-Route entry carries the cookie")
}

/// The UAS's `status` to the forwarded INVITE `fwd`: its Via stack and
/// Record-Route echoed (RFC 3261 §12.1.1), with the UAS's To-tag.
fn answer(fwd: &[u8], status: &str) -> SipResponse {
    let echoed: String = String::from_utf8_lossy(fwd)
        .lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("via:") || l.starts_with("record-route:")
        })
        .map(|l| format!("{l}\r\n"))
        .collect();
    let SipMessage::Response(resp) = parse(&format!(
        "SIP/2.0 {status}\r\n\
{echoed}From: <sip:b2bua@{W1_POD}>;tag=w\r\n\
To: <sip:bob@{UAS}>;tag=b\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@{UAS}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    )) else {
        unreachable!("a status line parses as a response")
    };
    resp
}

// The proxy departed w1 a moment before w1's INVITE reached it: the INVITE is
// still worker-outbound (its Via names a tombstoned worker address), and the
// cookie it is Record-Routed with names w1 and the alive backup w2, signed for
// the call. The UAS's 180 and 200 for the departed w1 fail over to w2.
#[tokio::test]
async fn a_departed_workers_invite_is_recorded_with_its_ordinal_and_an_alive_backup() {
    let f = fixture();
    f.registry.remove("w1");
    assert_eq!(
        f.registry.lookup_by_address(&ProxyAddr::new(W1_POD, 5060)).map(|w| w.health),
        Some(WorkerHealth::Dead),
        "w1's address is tombstoned"
    );

    let invite = parse(&initial_invite(W1_POD));
    f.core.handle_request_from(invite.clone(), format!("{W1_POD}:5060").parse().unwrap()).await;
    let (dst, fwd) = f.one_sent();
    assert_eq!(dst, format!("{UAS}:5060").parse::<SocketAddr>().unwrap(), "forwarded to the UAS");

    let cookie = recorded_cookie(&fwd);
    assert_eq!(cookie.get("w_pri").map(String::as_str), Some("w1"), "{cookie:?}");
    assert_eq!(cookie.get("w_bak").map(String::as_str), Some("w2"), "{cookie:?}");
    assert!(f.strategy.verified_cookie(&cookie, &invite).is_some(), "signed for the call");

    let w2: SocketAddr = format!("{W2_POD}:5060").parse().unwrap();
    for status in ["180 Ringing", "200 OK"] {
        f.core.handle_response_from(answer(&fwd, status), uas()).await;
        let (dst, _) = f.one_sent();
        assert_eq!(dst, w2, "{status} fails over to the backup");
    }
}

/// `w1`'s initial INVITE forwarded after `w1` departed, its 180 relayed at
/// +1 s; then the clock runs past the tombstone's window, as on a long ring.
/// The forwarded bytes.
async fn invite_rung_past_the_tombstone(f: &Fixture) -> Vec<u8> {
    f.registry.remove("w1");
    let invite = parse(&initial_invite(W1_POD));
    f.core.handle_request_from(invite, format!("{W1_POD}:5060").parse().unwrap()).await;
    let (_, fwd) = f.one_sent();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    f.core.handle_response_from(answer(&fwd, "180 Ringing"), uas()).await;
    let _ringing = f.one_sent();
    let window = std::time::Duration::from_millis(DEPARTED_ADDRESS_TTL_MS);
    tokio::time::advance(window + std::time::Duration::from_secs(7)).await;
    assert!(
        f.registry.lookup_by_address(&ProxyAddr::new(W1_POD, 5060)).is_none(),
        "w1's address is a stranger again"
    );
    fwd
}

fn uas() -> SocketAddr {
    format!("{UAS}:5060").parse().unwrap()
}

// The dialog, not the tombstone's window, carries the departed worker's
// INVITE: a 200 answered after the window lapsed still fails over to the
// cookie's backup, as do its retransmissions.
#[tokio::test(start_paused = true)]
async fn a_200_answered_past_the_tombstone_still_reaches_the_backup() {
    let f = fixture();
    let fwd = invite_rung_past_the_tombstone(&f).await;
    let w2: SocketAddr = format!("{W2_POD}:5060").parse().unwrap();
    for _ in 0..2 {
        f.core.handle_response_from(answer(&fwd, "200 OK"), uas()).await;
        assert_eq!(f.one_sent().0, w2, "the 200 fails over to the backup");
    }
}

// Past the window, a worker that holds the departed ordinal at a new address
// takes the INVITE's 200, as it would inside it.
#[tokio::test(start_paused = true)]
async fn a_200_answered_past_the_tombstone_reaches_the_ordinal_at_its_new_address() {
    let f = fixture();
    let fwd = invite_rung_past_the_tombstone(&f).await;
    f.registry.add(WorkerEntry::alive("w1", ProxyAddr::new(REPLACEMENT, 5060)));
    f.core.handle_response_from(answer(&fwd, "200 OK"), uas()).await;
    let replacement: SocketAddr = format!("{REPLACEMENT}:5060").parse().unwrap();
    assert_eq!(f.one_sent().0, replacement);
}

// Past the window, neither the cookie's worker nor its backup holds an
// address: the 200 goes nowhere, counted as dropped, and never along the Via
// to the departed address.
#[tokio::test(start_paused = true)]
async fn a_200_past_the_tombstone_with_no_usable_backup_is_dropped() {
    let f = fixture();
    let fwd = invite_rung_past_the_tombstone(&f).await;
    f.registry.set_health("w2", WorkerHealth::Dead);
    f.core.handle_response_from(answer(&fwd, "200 OK"), uas()).await;
    assert!(f.wire.sent.lock().unwrap().is_empty(), "nothing is sent");
    let text = f.metrics.prometheus_text();
    assert!(text.contains("sip_messages_total{label=\"outbound:dropped\"} 1\n"), "{text}");
}

// Past the window, another worker took the departed worker's address: the
// address names neither of the cookie's workers, so the cookie carries the 200
// to the backup, never to the address's new holder.
#[tokio::test(start_paused = true)]
async fn a_200_past_the_tombstone_skips_a_worker_that_took_the_address() {
    let f = fixture();
    let fwd = invite_rung_past_the_tombstone(&f).await;
    f.registry.add(WorkerEntry::alive("w5", ProxyAddr::new(W1_POD, 5060)));
    f.core.handle_response_from(answer(&fwd, "200 OK"), uas()).await;
    let w2: SocketAddr = format!("{W2_POD}:5060").parse().unwrap();
    assert_eq!(f.one_sent().0, w2, "the cookie's backup takes the 200");
}

// The cookie's backup serves the dialog as a takeover copy while its primary
// is still alive and sends a re-INVITE carrying the dialog's cookie: the
// address names the cookie's backup, so the 200 follows the Via to it, the
// node holding the transaction.
#[tokio::test]
async fn a_200_to_the_cookies_backup_follows_the_via() {
    let f = fixture();
    let invite = parse(&initial_invite(W1_POD));
    let cookie = f.strategy.encode_stickiness(&ProxyAddr::new(W1_POD, 5060), &invite).unwrap();
    let cookie_text: String = cookie.iter().map(|(k, v)| format!(";{k}={v}")).collect();
    let route = format!(
        "Route: <sip:{PROXY_VIP}:5060;outbound;lr>, <sip:{PROXY_VIP}:5060{cookie_text};lr>\r\n"
    );
    let reinvite = initial_invite(W2_POD)
        .replace(&format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n"), &route)
        .replace("To: <sip:bob@10.244.7.13>\r\n", "To: <sip:bob@10.244.7.13>;tag=b\r\n")
        .replace("CSeq: 1 INVITE", "CSeq: 2 INVITE");
    f.core.handle_request_from(parse(&reinvite), format!("{W2_POD}:5060").parse().unwrap()).await;
    let (_, fwd) = f.one_sent();
    f.core.handle_response_from(answer(&fwd, "200 OK"), uas()).await;
    let w2: SocketAddr = format!("{W2_POD}:5060").parse().unwrap();
    assert_eq!(f.one_sent().0, w2, "the 200 returns to the copy that sent the re-INVITE");
}

// An address neither in the worker set nor tombstoned names no worker: no
// cookie is minted for it.
#[tokio::test]
async fn an_address_no_worker_ever_held_gets_no_cookie() {
    let f = fixture();
    let invite = parse(&initial_invite(STRANGER));
    assert!(f.strategy.encode_stickiness(&ProxyAddr::new(STRANGER, 5060), &invite).is_none());
}
