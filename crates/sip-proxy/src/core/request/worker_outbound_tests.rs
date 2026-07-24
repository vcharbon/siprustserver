//! Worker-outbound classification: the SNAT-immune Via/registry
//! discriminators and the double-record-route direction contract.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

use crate::addr::ProxyAddr;
use crate::core::ProxyCoreBuilder;
use crate::observability::metrics::RoutingDecisionKind;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

// Worker w1 lives at its POD ip:5060 (what the registry holds and the worker
// stamps as its Via sent-by). The downstream UAC the keepalive targets.
const W1_POD: &str = "10.244.5.8";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "172.20.255.250";

async fn core(reg: Arc<dyn WorkerRegistry>) -> crate::core::ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(Arc::new(ProxyMetrics::new()))
        .build(ep)
}

// A B2BUA A-leg keepalive OPTIONS toward the UAC: top Via = the originating
// worker, our own cookie Route (`target=worker`, no `;outbound`), R-URI = the
// UAC. This is exactly the on-wire shape captured in the endurance repro.
fn keepalive_options() -> SipMessage {
    let raw = format!(
        "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKkeepalive;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-1@{UAC}\r\n\
CSeq: 2 OPTIONS\r\n\
Contact: <sip:b2bua@{W1_POD}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

// Regression for the steady-state long-call-loss class: behind the keepalived
// VIP a worker→proxy packet is SNAT'd to the NODE ip:ephemeral-port, so the
// proxy's UDP source is NOT a registered worker. The worker-outbound
// classification must therefore key off the SNAT-immune top Via sent-by, not
// the socket source — otherwise the cookie's `target=worker` decode bounces
// the keepalive straight back to a worker and the UAC never sees it (350s
// recv-timeout → BYE → 481).
#[tokio::test]
async fn snat_masqueraded_worker_keepalive_routes_to_downstream_not_back_to_worker() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    // SNAT'd source: the kind NODE ip + an ephemeral port — NOT in the registry.
    let snat_src = "172.20.0.11:63522".parse().unwrap();
    let msg = keepalive_options();
    let outcome = core.route_request(&msg, snat_src).await;

    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::WorkerOutbound,
        "a worker-originated keepalive must be worker-outbound even when SNAT hides the source"
    );
    assert_eq!(
        outcome.target,
        Some(ProxyAddr::new(UAC, 5060)),
        "the keepalive must reach the UAC (R-URI), not bounce back to a worker via the cookie"
    );
}

// Regression for the recv-loop head-of-line block: a worker-outbound
// request whose R-URI is a DNS name must NOT make routing wait on the
// resolver — resolution happens on a spawned task (see crate::resolver).
// Under an inline-await design a resolver that never answers would
// park route_request (and with it the whole recv loop) forever;
// here the only pending timer is the watchdog timeout, so a regression
// fails fast instead of hanging.
#[tokio::test(start_paused = true)]
async fn named_target_resolution_never_blocks_routing() {
    struct PendingResolver;
    #[async_trait::async_trait]
    impl crate::resolver::HostResolver for PendingResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> Option<std::net::SocketAddr> {
            std::future::pending().await
        }
    }

    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .resolver(Arc::new(PendingResolver))
        .build(ep);

    let raw = format!(
        "OPTIONS sip:sipp@uas.example:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKnamed;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@uas.example:5060>;tag=uactag\r\n\
Call-ID: named-1@uas.example\r\n\
CSeq: 2 OPTIONS\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        core.route_request(&msg, format!("{W1_POD}:5060").parse().unwrap()),
    )
    .await
    .expect("route_request must not wait on DNS resolution");
    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new("uas.example", 5060)));
}

// The un-NAT'd fast path still works: source IS the registered worker.
#[tokio::test]
async fn pod_direct_worker_source_is_still_worker_outbound() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    let pod_src = format!("{W1_POD}:5060").parse().unwrap();
    let msg = keepalive_options();
    let outcome = core.route_request(&msg, pod_src).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
}

// A genuine EXTERNAL in-dialog request (top Via = a non-worker UAC) must NOT
// be misclassified as worker-outbound — it follows the cookie back to its
// worker as before.
#[tokio::test]
async fn external_in_dialog_request_is_not_worker_outbound() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    // Same cookie Route, but the top Via sent-by is the UAC (not a worker).
    let raw = format!(
        "BYE sip:b2bua@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKext;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
To: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
Call-ID: longcall-1@{UAC}\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request(&msg, format!("{UAC}:5060").parse().unwrap()).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::DecodeForward);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
}

// A worker-stamped `;outbound` is accepted as backward-compatible
// defense-in-depth; the load-bearing path is the proxy's own
// double-record-route (tests below).
#[tokio::test]
async fn worker_stamped_outbound_marker_still_accepted() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    let new_pod_ip = "10.244.9.99"; // rebooted worker's new IP — NOT in registry
    let raw = format!(
        "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {new_pod_ip}:5060;branch=z9hG4bKreboot;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-2@{UAC}\r\n\
CSeq: 3 OPTIONS\r\n\
Contact: <sip:b2bua@{new_pod_ip}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr;outbound>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
    let snat_src = "172.20.0.12:51000".parse().unwrap();
    let outcome = core.route_request(&msg, snat_src).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
}

// ── Double-record-route: direction is intrinsic to the proxy's own RR ─────
//
// THE load-bearing case. A worker that has just rebooted onto a NEW pod IP
// (absent from the registry) sends, behind the SNAT'd VIP, its keepalive on
// the route set captured at dialog set-up: the proxy's OWN `;outbound`
// Record-Route on top, the stickiness cookie below. The worker stamps NOTHING.
// Neither the SNAT'd source nor the top Via (the new IP) is a registered
// worker, so every registry-based discriminator MISSES — yet direction is
// still correct because it is read from the proxy's own self-issued top RR.
// The worker's own `;outbound` stamp is therefore not load-bearing.
#[tokio::test]
async fn rebooted_worker_keepalive_direction_from_proxy_issued_outbound_rr() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    let new_pod_ip = "10.244.9.99"; // rebooted worker's new IP — NOT in registry
    let raw = format!(
        "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {new_pod_ip}:5060;branch=z9hG4bKreboot;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-3@{UAC}\r\n\
CSeq: 4 OPTIONS\r\n\
Contact: <sip:b2bua@{new_pod_ip}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
    let snat_src = "172.20.0.12:51000".parse().unwrap(); // node IP, not a worker
    let outcome = core.route_request(&msg, snat_src).await;

    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::WorkerOutbound,
        "direction must come from the proxy's own top `;outbound` RR — no worker marker, no registry/Via match"
    );
    assert_eq!(
        outcome.target,
        Some(ProxyAddr::new(UAC, 5060)),
        "the keepalive must reach the UAC (R-URI), not bounce back via the cookie below"
    );
}

// The mirror direction with the double-record-route present: an EXTERNAL
// in-dialog request from the UAC carries the cookie on top (alice's route set
// is the reverse of the 2xx: cookie, then outbound). The proxy pops BOTH self
// RRs but reads direction from the top (cookie) → decode to the worker. The
// trailing `;outbound` half must NOT flip it to worker-outbound.
#[tokio::test]
async fn external_in_dialog_with_double_rr_decodes_to_worker() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = core(reg).await;

    let raw = format!(
        "BYE sip:b2bua@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKext;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
To: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
Call-ID: longcall-3@{UAC}\r\n\
CSeq: 5 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request(&msg, format!("{UAC}:5060").parse().unwrap()).await;

    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::DecodeForward,
        "cookie on top → decode to the worker; the trailing ;outbound half must not flip direction"
    );
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
}
