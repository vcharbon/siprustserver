//! A proxy before its first worker view answers nothing it cannot route
//! (ADR-0050 §5, §11): a new call, an in-dialog request and a CANCEL with no
//! alive target are dropped and counted, never answered `503`; the sender's
//! retransmission meets the view. Once the view is in, an alive worker takes
//! them, and a new call with no alive worker is refused as before.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::health::FirstView;
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::observability::metrics::NoTargetDrop;
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerHealth};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::LoadBalancerStrategy;
use crate::strategy::{RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const CALL_ID: &str = "boot-1@10.244.7.13";

struct Fixture {
    core: ProxyCore,
    strategy: Arc<LoadBalancerStrategy>,
    registry: SimulatedWorkerRegistry,
    metrics: Arc<ProxyMetrics>,
    view: Arc<FirstView>,
}

/// A starting proxy over two workers it has not yet read: both `Unknown`,
/// its first view pending.
async fn starting() -> Fixture {
    let clock = Clock::test_at(0);
    let entry = |id: &str, host: &str| WorkerEntry {
        health: WorkerHealth::Unknown,
        ..WorkerEntry::alive(id, ProxyAddr::new(host, 5060))
    };
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![entry("w1", W1_POD), entry("w2", W2_POD)],
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
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let view = FirstView::pending();
    let core = ProxyCoreBuilder::for_test(
        ProxyAddr::new(PROXY_VIP, 5060),
        strategy.clone(),
        Arc::new(registry.clone()),
    )
    .clock(clock)
    .metrics(metrics.clone())
    .first_view(view.clone())
    .build(ep);
    Fixture { core, strategy, registry, metrics, view }
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn src() -> SocketAddr {
    format!("{UAC}:5060").parse().unwrap()
}

/// The caller's out-of-dialog `method`, `branch` its Via branch.
fn initial(method: &str, branch: &str) -> SipMessage {
    parse(&format!(
        "{method} sip:bob@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 {method}\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The proxy's own Record-Route URI carrying the cookie for `primary`.
fn cookie_uri(f: &Fixture, primary: &str) -> String {
    let params: RouteParams = f
        .strategy
        .encode_stickiness(&ProxyAddr::new(primary, 5060), &initial("INVITE", "z9hG4bKinv"))
        .unwrap();
    let params: String = params.iter().map(|(k, v)| format!(";{k}={v}")).collect();
    format!("sip:{PROXY_VIP}:5060;lr{params}")
}

/// The caller's in-dialog BYE, `route` its Route URI.
fn bye(route: &str) -> SipMessage {
    parse(&format!(
        "BYE sip:bob@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKbye;rport\r\n\
Route: <{route}>\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>;tag=b\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// Nothing was answered, and nothing refused for want of a target.
fn assert_unanswered(f: &Fixture) {
    let text = f.metrics.prometheus_text();
    assert!(text.contains("\nsip_messages_total{label=\"outbound:responded\"} 0\n"), "no answer");
    assert_eq!(f.metrics.reject_count("no_target_available"), 0, "never a 503");
}

/// A new call before the first view: dropped and counted, never a 503.
#[tokio::test]
async fn a_new_call_before_the_first_view_is_dropped_unanswered() {
    let f = starting().await;
    let out = f.core.route_request_from(&initial("INVITE", "z9hG4bKinv"), src()).await;
    assert_eq!(out.target, None);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::Initial), 1);
    assert_unanswered(&f);
}

/// An in-dialog request before the first view, its cookie read or not, and a
/// CANCEL whose INVITE this proxy never saw: dropped and counted, never a
/// 503.
#[tokio::test]
async fn in_dialog_requests_and_cancels_before_the_first_view_are_dropped_unanswered() {
    let f = starting().await;
    let cookie = cookie_uri(&f, W1_POD);
    assert_eq!(f.core.route_request_from(&bye(&cookie), src()).await.target, None);
    let unreadable = format!("sip:{PROXY_VIP}:5060;lr");
    assert_eq!(f.core.route_request_from(&bye(&unreadable), src()).await.target, None);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::InDialog), 2);
    let cancel = initial("CANCEL", "z9hG4bKinv");
    assert_eq!(f.core.route_request_from(&cancel, src()).await.target, None);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::Cancel), 1);
    assert_unanswered(&f);
}

/// The view in, with an alive worker: a new call and an in-dialog request
/// are routed.
#[tokio::test]
async fn after_the_first_view_an_alive_worker_takes_them() {
    let f = starting().await;
    f.registry.set_health("w1", WorkerHealth::Alive);
    f.view.latch();
    let w1 = Some(ProxyAddr::new(W1_POD, 5060));
    let invite = initial("INVITE", "z9hG4bKinv");
    assert_eq!(f.core.route_request_from(&invite, src()).await.target, w1);
    let cookie = cookie_uri(&f, W1_POD);
    assert_eq!(f.core.route_request_from(&bye(&cookie), src()).await.target, w1);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::Initial), 0);
}

/// The view in and no worker alive: a new call is refused, as by any proxy
/// that has read its workers.
#[tokio::test]
async fn after_the_first_view_a_new_call_with_no_alive_worker_is_refused() {
    let f = starting().await;
    f.view.latch();
    let out = f.core.route_request_from(&initial("INVITE", "z9hG4bKinv"), src()).await;
    assert_eq!(out.target, None);
    assert_eq!(f.metrics.reject_count("no_target_available"), 1);
    assert_eq!(f.metrics.no_target_dropped_count(NoTargetDrop::Initial), 0);
}
