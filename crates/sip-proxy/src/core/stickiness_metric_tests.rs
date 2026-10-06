//! What a cookie decode says about the primary is counted once per request
//! routed by the cookie (an in-dialog request, or a CANCEL following its
//! INVITE's cookie), and never on the reverse-failover response path, which
//! decodes the same cookie but forwards only to a backup.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerHealth};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::{LoadBalancerConfig, LoadBalancerStrategy};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy};
use crate::ProxyMetrics;

const W1_POD: &str = "10.244.5.8";
const W2_POD: &str = "10.244.5.9";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const CALL_ID: &str = "dlg-1@10.244.7.13";

struct Fixture {
    core: ProxyCore,
    strategy: Arc<LoadBalancerStrategy>,
    registry: SimulatedWorkerRegistry,
    metrics: Arc<ProxyMetrics>,
}

/// A proxy over the load balancer and two workers, `w1` and `w2`, both
/// freshly seen: inside their fresh-pod guard window for the whole test.
async fn fixture() -> Fixture {
    let clock = Clock::test_at(0);
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry {
                first_seen_at_ms: Some(0),
                ..WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))
            },
            WorkerEntry {
                first_seen_at_ms: Some(0),
                ..WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060))
            },
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
        LoadBalancerConfig::default(),
    ));
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let core = ProxyCoreBuilder::new(
        ProxyAddr::new(PROXY_VIP, 5060),
        strategy.clone(),
        Arc::new(registry.clone()),
    )
    .clock(clock)
    .metrics(metrics.clone())
    .build(ep);
    Fixture { core, strategy, registry, metrics }
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

/// The proxy's own Record-Route URI carrying the cookie for `primary`.
fn cookie_uri(f: &Fixture, primary: &str) -> String {
    let invite = parse(&format!(
        "INVITE sip:bob@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let params: RouteParams =
        f.strategy.encode_stickiness(&ProxyAddr::new(primary, 5060), &invite).unwrap();
    let params: String = params.iter().map(|(k, v)| format!(";{k}={v}")).collect();
    format!("sip:{PROXY_VIP}:5060;lr{params}")
}

/// The caller's in-dialog BYE, routed by the cookie.
fn bye(cookie: &str, cseq: u32) -> SipMessage {
    parse(&format!(
        "BYE sip:bob@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKbye{cseq};rport\r\n\
Route: <{cookie}>\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>;tag=b\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: {cseq} BYE\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn renders(metrics: &ProxyMetrics, line: &str) -> bool {
    metrics.prometheus_text().contains(&format!("\n{line}\n"))
}

// Each decode fact moves only its own series: a fresh primary passed over
// for the backup, a not-ready primary passed over, and a fresh primary
// served for want of a usable backup.
#[tokio::test]
async fn each_request_decode_counts_what_it_says_of_the_primary() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W1_POD);
    let src = format!("{UAC}:5060").parse().unwrap();
    let w1 = Some(ProxyAddr::new(W1_POD, 5060));
    let w2 = Some(ProxyAddr::new(W2_POD, 5060));

    let out = f.core.route_request(&bye(&cookie, 3), src).await;
    assert_eq!(out.target, w2, "fresh w1: to the backup");
    f.registry.set_health("w1", WorkerHealth::NotReady);
    for cseq in 4..6 {
        let out = f.core.route_request(&bye(&cookie, cseq), src).await;
        assert_eq!(out.target, w2, "not-ready w1: to the backup");
    }
    f.registry.set_health("w1", WorkerHealth::Alive);
    f.registry.set_health("w2", WorkerHealth::Dead);
    for cseq in 6..9 {
        let out = f.core.route_request(&bye(&cookie, cseq), src).await;
        assert_eq!(out.target, w1, "no usable backup: the fresh w1");
    }

    let m = &f.metrics;
    assert!(renders(m, "sip_proxy_decode_forward_promotions_total{reason=\"fresh_pod\"} 1"));
    assert!(renders(m, "sip_proxy_decode_forward_promotions_total{reason=\"not_ready\"} 2"));
    assert!(renders(m, "sip_proxy_fresh_pod_forwards_total 3"));
}

// A CANCEL follows its INVITE's cookie: with the INVITE's primary not ready
// it goes to the backup, counted as a not-ready promotion.
#[tokio::test]
async fn a_cancel_routed_by_its_invites_cookie_counts_its_promotion() {
    let f = fixture().await;
    let src = format!("{UAC}:5060").parse().unwrap();
    let invite = parse(&format!(
        "INVITE sip:bob@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let primary = f.core.route_request(&invite, src).await.target.expect("the INVITE is routed");
    let (primary_id, backup) = if primary.host == W1_POD {
        ("w1", ProxyAddr::new(W2_POD, 5060))
    } else {
        ("w2", ProxyAddr::new(W1_POD, 5060))
    };
    f.registry.set_health(primary_id, WorkerHealth::NotReady);

    let cancel = parse(&format!(
        "CANCEL sip:bob@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=a\r\n\
To: <sip:bob@{UAC}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 CANCEL\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let out = f.core.route_request(&cancel, src).await;
    assert_eq!(out.target, Some(backup), "the CANCEL follows the cookie to the backup");

    let m = &f.metrics;
    assert!(renders(m, "sip_proxy_cancel_lookups_total{outcome=\"hit\"} 1"));
    assert!(renders(m, "sip_proxy_decode_forward_promotions_total{reason=\"not_ready\"} 1"));
    assert!(renders(m, "sip_proxy_decode_forward_promotions_total{reason=\"fresh_pod\"} 0"));
}

// A response from a dead worker whose cookie names a fresh primary and that
// dead worker as backup decodes to the fresh primary; the response path
// drops it, and nothing was forwarded to the fresh primary.
#[tokio::test]
async fn a_dropped_reverse_failover_response_counts_no_fresh_pod_forward() {
    let f = fixture().await;
    let cookie = cookie_uri(&f, W2_POD);
    assert!(cookie.contains("w_pri=w2") && cookie.contains("w_bak=w1"), "{cookie}");
    // w2 is the fresh primary from here on; w1, the backup, dies.
    f.registry.set_health("w1", WorkerHealth::Dead);
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKw1;rport\r\n\
Record-Route: <{cookie}>\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:sipp@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = parse(&raw) else { unreachable!() };
    let params = crate::headers::cookie_params(
        resp.list::<sip_message::header::RecordRouteEntry>().unwrap()[0].uri(),
    );
    let decoded = f.strategy.decode_stickiness(&params, &SipMessage::Response(resp.clone())).await;
    assert!(matches!(decoded, DecodeResult::Forward { fresh_primary: true, .. }), "{decoded:?}");
    f.core.handle_response(resp, format!("{W1_POD}:5060").parse().unwrap()).await;

    let m = &f.metrics;
    assert!(
        renders(m, "sip_messages_total{label=\"outbound:dropped\"} 1"),
        "the response path dropped it"
    );
    assert!(renders(m, "sip_proxy_fresh_pod_forwards_total 0"));
    assert!(renders(m, "sip_proxy_decode_forward_promotions_total{reason=\"fresh_pod\"} 0"));
}
