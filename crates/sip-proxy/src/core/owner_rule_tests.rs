//! One routing rule for every method, over three workers (ADR-0055 §8): a
//! worker's dialog names the backup the worker states, a worker's request
//! carries its cookie on the pushed Via whatever its method, and a response
//! goes to the owner that holds its transaction, else to the call's other
//! owner, else nowhere.

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
const W3_POD: &str = "10.244.5.10";
const UAS: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";

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
    wire: Arc<Wire>,
    /// A Call-ID for which the proxy's own pick of `w1`'s backup is `w2`.
    call_id: String,
}

/// A proxy over the load balancer and three alive workers.
fn fixture() -> Fixture {
    let clock = Clock::test_at(0);
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060)),
            WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
            WorkerEntry::alive("w3", ProxyAddr::new(W3_POD, 5060)),
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
    let call_id = (0..64)
        .map(|i| format!("owner-{i}@{UAS}"))
        .find(|id| {
            let invite = parse(&request(id, W1_POD, "", "INVITE", "z9hG4bKpick", "", "", "1"));
            strategy
                .encode_stickiness(&ProxyAddr::new(W1_POD, 5060), &invite)
                .is_some_and(|c| c["w_bak"] == "w2")
        })
        .expect("a Call-ID the proxy pairs w1 with w2");
    Fixture { core, strategy, registry, wire, call_id }
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn at(pod: &str) -> SocketAddr {
    format!("{pod}:5060").parse().unwrap()
}

/// A request the worker at `pod` sends toward the UAS through the proxy;
/// `bak` is the backup it states on its Via (`""`: none).
#[allow(clippy::too_many_arguments)]
fn request(
    call_id: &str,
    pod: &str,
    bak: &str,
    method: &str,
    branch: &str,
    route: &str,
    to_tag: &str,
    seq: &str,
) -> String {
    let stated = if bak.is_empty() { String::new() } else { format!(";bak={bak}") };
    format!(
        "{method} sip:bob@{UAS}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {pod}:5060;branch={branch};rport{stated}\r\n\
{route}Max-Forwards: 70\r\n\
From: <sip:b2bua@{pod}>;tag=w\r\n\
To: <sip:bob@{UAS}>{to_tag}\r\n\
Call-ID: {call_id}\r\n\
CSeq: {seq} {method}\r\n\
Content-Length: 0\r\n\r\n"
    )
}

/// The proxy's preloaded Route, as a worker's initial request carries it.
fn preloaded() -> String {
    format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n")
}

/// The dialog's route set as a worker states it: the proxy's worker-facing
/// `;outbound` half, then its cookie half.
fn dialog_route(cookie: &RouteParams) -> String {
    let field = |(k, v): (&String, &String)| {
        if v.is_empty() {
            format!(";{k}")
        } else {
            format!(";{k}={v}")
        }
    };
    let text: String = cookie.iter().map(field).collect();
    format!("Route: <sip:{PROXY_VIP}:5060;outbound;lr>, <sip:{PROXY_VIP}:5060{text};lr>\r\n")
}

/// The single datagram the last step put on the wire.
fn one_sent(f: &Fixture) -> (SocketAddr, Vec<u8>) {
    let mut sent = std::mem::take(&mut *f.wire.sent.lock().unwrap());
    assert_eq!(sent.len(), 1, "one datagram");
    sent.remove(0)
}

fn line(bytes: &[u8], header: &str, holding: &str) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with(header) && l.contains(holding))
        .map(str::to_string)
}

fn top_via(bytes: &[u8]) -> String {
    line(bytes, "via:", "").expect("a Via")
}

/// The UAS's `status` to the forwarded request `fwd`: its Via stack echoed,
/// no Record-Route.
fn answer(f: &Fixture, fwd: &[u8], status: &str, method: &str, seq: &str) -> SipResponse {
    let vias: String = String::from_utf8_lossy(fwd)
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
        .map(|l| format!("{l}\r\n"))
        .collect();
    let SipMessage::Response(resp) = parse(&format!(
        "SIP/2.0 {status}\r\n\
{vias}From: <sip:b2bua@{W1_POD}>;tag=w\r\n\
To: <sip:bob@{UAS}>;tag=b\r\n\
Call-ID: {}\r\n\
CSeq: {seq} {method}\r\n\
Content-Length: 0\r\n\r\n",
        f.call_id
    )) else {
        unreachable!("a status line parses as a response")
    };
    resp
}

/// The call's cookie as the proxy signs it for a dialog `pod`'s worker
/// originates, stating `bak`: read off the Record-Route of its INVITE.
async fn dialog_cookie(f: &Fixture, pod: &str, bak: &str) -> RouteParams {
    let invite = request(&f.call_id, pod, bak, "INVITE", "z9hG4bKdlg", &preloaded(), "", "1");
    f.core.handle_request_from(parse(&invite), at(pod)).await;
    let (_, fwd) = one_sent(f);
    let rr = line(&fwd, "record-route:", "sig=").expect("the cookie half");
    let uri = rr.split('<').nth(1).unwrap().split('>').next().unwrap();
    let uri = sip_message::header::Uri::parse(&sip_message::SipStr::owned(uri)).unwrap();
    let all = crate::headers::cookie_params(&uri);
    f.strategy.verified_cookie(&all, &parse(&invite)).expect("the minted cookie verifies")
}

fn uas() -> SocketAddr {
    at(UAS)
}

// A worker's dialog names the backup the worker states on its Via, never the
// proxy's own pick over that dialog's Call-ID: every leg of one call names
// the same backup, the one holding the call's replica.
#[tokio::test]
async fn a_workers_dialog_names_the_backup_it_states() {
    let f = fixture();
    let invite = request(&f.call_id, W1_POD, "w3", "INVITE", "z9hG4bKini", &preloaded(), "", "1");
    f.core.handle_request_from(parse(&invite), at(W1_POD)).await;
    let (_, fwd) = one_sent(&f);
    let rr = line(&fwd, "record-route:", "sig=").expect("the cookie half");
    assert!(rr.contains("w_pri=w1") && rr.contains("w_bak=w3"), "{rr}");
    let via = top_via(&fwd);
    assert!(via.contains("w_pri=w1") && via.contains("w_bak=w3"), "{via}");
}

// A worker the proxy has departed still names the backup it states.
#[tokio::test]
async fn a_departed_workers_dialog_names_the_backup_it_states() {
    let f = fixture();
    f.registry.remove("w1");
    let invite = request(&f.call_id, W1_POD, "w3", "INVITE", "z9hG4bKdep", &preloaded(), "", "1");
    f.core.handle_request_from(parse(&invite), at(W1_POD)).await;
    let (_, fwd) = one_sent(&f);
    let rr = line(&fwd, "record-route:", "sig=").expect("the cookie half");
    assert!(rr.contains("w_pri=w1") && rr.contains("w_bak=w3"), "{rr}");
}

// A call with one owner: the worker states no backup, and the cookie names
// none.
#[tokio::test]
async fn a_workers_dialog_with_one_owner_names_no_backup() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "").await;
    assert_eq!(cookie["w_pri"], "w1");
    assert_eq!(cookie["w_bak"], "", "{cookie:?}");
}

// A worker's in-dialog request of any method carries the dialog's cookie on
// the Via the proxy pushes.
#[tokio::test]
async fn a_workers_request_of_any_method_carries_the_cookie_on_the_pushed_via() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "w3").await;
    for (method, seq) in [("BYE", "2"), ("OPTIONS", "3"), ("INFO", "4"), ("UPDATE", "5")] {
        let raw = request(
            &f.call_id,
            W1_POD,
            "w3",
            method,
            &format!("z9hG4bK{method}"),
            &dialog_route(&cookie),
            ";tag=b",
            seq,
        );
        f.core.handle_request_from(parse(&raw), at(W1_POD)).await;
        let via = top_via(&one_sent(&f).1);
        assert!(via.contains(&format!(";sig={}", cookie["sig"])), "{method}: {via}");
    }
}

/// `pod`'s worker sends `method` in the dialog `cookie` names, and the
/// proxy forwards it: the forwarded bytes.
async fn sent_in_dialog(
    f: &Fixture,
    pod: &str,
    bak: &str,
    cookie: &RouteParams,
    method: &str,
    seq: &str,
) -> Vec<u8> {
    let raw = request(
        &f.call_id,
        pod,
        bak,
        method,
        &format!("z9hG4bK{method}{seq}"),
        &dialog_route(cookie),
        ";tag=b",
        seq,
    );
    f.core.handle_request_from(parse(&raw), at(pod)).await;
    one_sent(f).1
}

// A response goes to the owner that holds its transaction while it is
// alive, whatever its method.
#[tokio::test]
async fn a_response_goes_to_the_owner_holding_its_transaction() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "w3").await;
    for (method, seq) in [("BYE", "2"), ("INVITE", "3")] {
        let fwd = sent_in_dialog(&f, W1_POD, "w3", &cookie, method, seq).await;
        f.core.handle_response_from(answer(&f, &fwd, "200 OK", method, seq), uas()).await;
        assert_eq!(one_sent(&f).0, at(W1_POD), "{method}");
    }
}

// A response whose holder is dead goes to the call's other owner, whatever its
// method: the backup the dialog names, which holds the call's replica.
#[tokio::test]
async fn a_response_for_a_dead_holder_goes_to_the_other_owner_whatever_its_method() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "w3").await;
    for (method, seq) in [("BYE", "2"), ("INFO", "3"), ("INVITE", "4")] {
        let fwd = sent_in_dialog(&f, W1_POD, "w3", &cookie, method, seq).await;
        f.registry.set_health("w1", WorkerHealth::Dead);
        f.core.handle_response_from(answer(&f, &fwd, "200 OK", method, seq), uas()).await;
        assert_eq!(one_sent(&f).0, at(W3_POD), "{method}");
        f.registry.set_health("w1", WorkerHealth::Alive);
    }
}

// A copy on the backup sends a request in the primary's dialog; the copy dies
// before its answer: the answer goes to the call's other owner, the primary.
#[tokio::test]
async fn a_response_for_a_dead_copy_goes_to_the_primary() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "w3").await;
    for (method, seq) in [("BYE", "2"), ("INVITE", "3")] {
        let fwd = sent_in_dialog(&f, W3_POD, "w1", &cookie, method, seq).await;
        f.registry.set_health("w3", WorkerHealth::Dead);
        f.core.handle_response_from(answer(&f, &fwd, "200 OK", method, seq), uas()).await;
        assert_eq!(one_sent(&f).0, at(W1_POD), "{method}");
        f.registry.set_health("w3", WorkerHealth::Alive);
    }
}

// A call with one owner: its response goes to it while it is alive, and
// nowhere once it is dead.
#[tokio::test]
async fn a_response_for_a_dead_sole_owner_goes_nowhere() {
    let f = fixture();
    let cookie = dialog_cookie(&f, W1_POD, "").await;
    for (method, seq) in [("BYE", "2"), ("INVITE", "3")] {
        let fwd = sent_in_dialog(&f, W1_POD, "", &cookie, method, seq).await;
        f.registry.set_health("w1", WorkerHealth::Dead);
        f.core.handle_response_from(answer(&f, &fwd, "200 OK", method, seq), uas()).await;
        assert!(f.wire.sent.lock().unwrap().is_empty(), "{method}: dropped");
        f.registry.set_health("w1", WorkerHealth::Alive);
    }
}
