//! Worker-outbound classification: the SNAT-immune Via/registry
//! discriminators and the double-record-route direction contract.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::{ExternalFaceParts, ProxyCoreBuilder};
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::observability::metrics::{Face, RoutingDecisionKind, UntrustedClaim};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::strategies::LoadBalancerStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

const W1_POD: &str = "10.244.5.8";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
/// The external face of the dual-face core.
const EXT_VIP: &str = "198.51.100.250";

async fn core(reg: Arc<dyn WorkerRegistry>) -> crate::core::ProxyCore {
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    core_routing(reg, strategy).await
}

async fn core_routing(
    reg: Arc<dyn WorkerRegistry>,
    strategy: Arc<dyn RoutingStrategy>,
) -> crate::core::ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(Arc::new(ProxyMetrics::new()))
        .build(ep)
}

/// A worker's in-dialog keepalive OPTIONS toward the UAC: the worker's top Via,
/// the proxy's own cookie Route (`target=` the worker, no `;outbound`), the UAC
/// as Request-URI.
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
    CustomParser::new().parse(raw.as_bytes()).unwrap()
}

/// A worker's request translated by a masquerading node arrives from an address
/// no registry holds: it is worker-outbound by its top Via sent-by, and reaches
/// the Request-URI instead of following the cookie back to a worker.
#[tokio::test]
async fn snat_masqueraded_worker_keepalive_routes_to_downstream_not_back_to_worker() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let core = core(reg).await;

    let snat_src = "192.0.2.11:63522".parse().unwrap();
    let msg = keepalive_options();
    let outcome = core.route_request_from(&msg, snat_src).await;

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

/// A worker-outbound request to a DNS name is routed without waiting on the
/// resolver: with a resolver that never answers, routing returns before the
/// 1 s watchdog.
#[tokio::test(start_paused = true)]
async fn named_target_resolution_never_blocks_routing() {
    struct PendingResolver;
    #[async_trait::async_trait]
    impl crate::resolver::HostResolver for PendingResolver {
        async fn resolve(
            &self,
            _host: &str,
            _port: u16,
            _deadline: tokio::time::Instant,
        ) -> Vec<std::net::SocketAddr> {
            std::future::pending().await
        }
    }

    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
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
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        core.route_request_from(&msg, format!("{W1_POD}:5060").parse().unwrap()),
    )
    .await
    .expect("route_request must not wait on DNS resolution");
    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new("uas.example", 5060)));
}

/// RFC 3261 §16.3: a Request-URI of a scheme that names no host is answered
/// 416, never forwarded.
#[tokio::test]
async fn a_worker_outbound_request_uri_naming_no_host_is_refused() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    let metrics = Arc::new(ProxyMetrics::new());
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(ep);
    let raw = format!(
        "OPTIONS urn:service:sos SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKurn;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sos@example.com>\r\n\
Call-ID: urn-1@{W1_POD}\r\n\
CSeq: 2 OPTIONS\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();

    let outcome = core.route_request_from(&msg, format!("{W1_POD}:5060").parse().unwrap()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
    assert_eq!(outcome.target, None);
    assert_eq!(metrics.reject_count("unsupported_uri_scheme"), 1);
}

#[tokio::test]
async fn pod_direct_worker_source_is_still_worker_outbound() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let core = core(reg).await;

    let pod_src = format!("{W1_POD}:5060").parse().unwrap();
    let msg = keepalive_options();
    let outcome = core.route_request_from(&msg, pod_src).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
}

#[tokio::test]
async fn external_in_dialog_request_is_not_worker_outbound() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let core = core(reg).await;

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
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request_from(&msg, format!("{UAC}:5060").parse().unwrap()).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::DecodeForward);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
}

/// A dual-face core: the internal face on `PROXY_VIP`, the external one on
/// `EXT_VIP`.
async fn dual_face_core(reg: Arc<dyn WorkerRegistry>) -> crate::core::ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |addr: &str| {
        let net = net.clone();
        let addr = addr.parse().unwrap();
        async move { net.bind_udp(BindUdpOpts::new(addr, 64)).await.unwrap() }
    };
    let int_ep = bind(&format!("{PROXY_VIP}:5060")).await;
    let ext_ep = bind(&format!("{EXT_VIP}:5060")).await;
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
    ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(Arc::new(ProxyMetrics::new()))
        .external_face(ExternalFaceParts {
            endpoint: ext_ep,
            advertised: ProxyAddr::new(EXT_VIP, 5060),
        })
        .build(int_ep)
}

/// An unsigned `;outbound` on the proxy's own Route entry, from a source and a
/// Via no registry holds: the internal face trusts it, the external face
/// ignores it, counts it, and routes the request by the entry's cookie.
#[tokio::test]
async fn an_unsigned_outbound_marker_is_trusted_only_on_the_internal_face() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let core = dual_face_core(reg).await;

    let new_pod_ip = "10.244.9.99";
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
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();
    let snat_src = "192.0.2.12:51000".parse().unwrap();

    let internal = core.route_request(&msg, snat_src, Face::Internal).await;
    assert_eq!(internal.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(internal.target, Some(ProxyAddr::new(UAC, 5060)));

    let external = core.route_request(&msg, snat_src, Face::External).await;
    assert_eq!(external.decision, RoutingDecisionKind::DecodeForward);
    assert_eq!(external.target, Some(ProxyAddr::new(W1_POD, 5060)));
    assert_eq!(core.metrics().external_untrusted_total(UntrustedClaim::OutboundMarker), 1);
}

/// A worker on a pod address no registry holds, behind address translation,
/// sends its keepalive on the route set of the dialog's set-up: the proxy's own
/// `;outbound` Record-Route on top, the cookie below, nothing stamped by the
/// worker. No registry match exists, and the direction is read from the
/// proxy's own top entry: worker-outbound, to the Request-URI.
#[tokio::test]
async fn rebooted_worker_keepalive_direction_from_proxy_issued_outbound_rr() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
    let core = core(reg).await;

    let new_pod_ip = "10.244.9.99";
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
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();
    let snat_src = "192.0.2.12:51000".parse().unwrap();
    let outcome = core.route_request_from(&msg, snat_src).await;

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

/// The caller's in-dialog request on a double Record-Route carries the cookie
/// on top and `;outbound` below: the proxy pops both own entries, reads the
/// direction from the top one, and forwards to the worker.
#[tokio::test]
async fn external_in_dialog_with_double_rr_decodes_to_worker() {
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(W1_POD, 5060),
        )]));
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
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request_from(&msg, format!("{UAC}:5060").parse().unwrap()).await;

    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::DecodeForward,
        "cookie on top → decode to the worker; the trailing ;outbound half must not flip direction"
    );
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
}

const W2_POD: &str = "10.244.5.9";
/// A third-party proxy the route set may cross between two passes.
const THIRD_PARTY: &str = "10.244.9.1";

/// A re-INVITE from the worker holding call 1, toward call 2, a call the
/// same dialog's INVITE spiralled into (§16.3): its route set holds this
/// proxy's pair recorded relaying it out of w1, then `between`, then the pair
/// recorded relaying it in to w2. The Request-URI is call 2's Contact at the
/// pod that held it before a takeover.
fn spiralled_reinvite(between: &str) -> SipMessage {
    CustomParser::new().parse(spiralled_reinvite_text(between).as_bytes()).unwrap()
}

fn spiralled_reinvite_text(between: &str) -> String {
    format!(
        "INVITE sip:b2bua@10.244.5.77:5060;leg=a SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKspiral;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{PROXY_VIP}>;tag=c1b\r\n\
To: <sip:bob@{PROXY_VIP}>;tag=c2a\r\n\
Call-ID: b-1@{W1_POD}\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:b2bua@{W1_POD}:5060;leg=b-1>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
{between}\
Route: <sip:{PROXY_VIP}:5060;target={W2_POD}:5060;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
    )
}

fn two_workers() -> Arc<dyn WorkerRegistry> {
    Arc::new(StaticWorkerRegistry::from_entries(vec![
        WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060)),
        WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
    ]))
}

/// Two own pairs back to back: the request leaves w1 through the first pair and
/// enters call 2 through the second, whose cookie, signed for this call,
/// decides; the first pair's `;outbound` would send it to a pod no longer
/// serving.
#[tokio::test]
async fn a_route_set_with_two_own_pairs_back_to_back_follows_the_last_pair() {
    let registry = two_workers();
    let strategy = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Arc::new(ProxyMetrics::new()),
        Clock::test_at(0),
    ));
    let core = core_routing(registry, strategy.clone()).await;
    let of_the_call = spiralled_reinvite("");
    let cookie = |worker: &str| -> String {
        strategy
            .encode_stickiness(&ProxyAddr::new(worker, 5060), &of_the_call)
            .expect("a cookie")
            .iter()
            .map(|(k, v)| format!(";{k}={v}"))
            .collect()
    };
    let signed = spiralled_reinvite_text("")
        .replace(&format!(";target={W1_POD}:5060"), &cookie(W1_POD))
        .replace(&format!(";target={W2_POD}:5060"), &cookie(W2_POD));
    let msg = CustomParser::new().parse(signed.as_bytes()).unwrap();
    let outcome = core.route_request_from(&msg, format!("{W1_POD}:5060").parse().unwrap()).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::DecodeForward);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W2_POD, 5060)));
}

/// A Record-Routing hop between two passes ends the run of own entries at the
/// first pair: worker-outbound, loose-routed to that hop.
#[tokio::test]
async fn a_third_party_between_two_passes_keeps_the_first_pair_direction() {
    let core = core(two_workers()).await;
    let between = format!("Route: <sip:{THIRD_PARTY}:5060;lr>\r\n");
    let outcome = core
        .route_request_from(
            &spiralled_reinvite(&between),
            format!("{W1_POD}:5060").parse().unwrap(),
        )
        .await;

    assert_eq!(outcome.decision, RoutingDecisionKind::LooseRoute);
    assert_eq!(outcome.target, Some(ProxyAddr::new(THIRD_PARTY, 5060)));
}

/// An odd run of own entries is no run of recorded pairs: the first entry
/// decides, and the worker Via makes the request worker-outbound.
#[tokio::test]
async fn an_odd_run_of_own_entries_from_a_worker_goes_to_the_request_uri() {
    let core = core(two_workers()).await;
    let raw = format!(
        "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKodd;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: odd-1@{UAC}\r\n\
CSeq: 2 OPTIONS\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let msg = CustomParser::new().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request_from(&msg, format!("{W1_POD}:5060").parse().unwrap()).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
}
