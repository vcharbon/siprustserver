//! The supervision OPTIONS (ADR-0069): an out-of-dialog OPTIONS to the
//! proxy's own address with no user part is answered `200` by the proxy while
//! a worker reads `Alive` or `Slow`, stating what the workers last advertised,
//! and dropped unanswered while none does or before the first view. An OPTIONS
//! with a user part, or any OPTIONS with the answer turned off, is relayed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipHeader;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::health::{FirstView, WorkerCapabilities};
use crate::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use crate::observability::metrics::LocalOptions;
use crate::registry::simulated::SimulatedWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerHealth};
use crate::security::hmac::{HmacKey, StaticHmacKeyProvider};
use crate::strategies::LoadBalancerStrategy;
use crate::ProxyMetrics;

const W1: &str = "192.0.2.21";
const W2: &str = "192.0.2.22";
const SBC: &str = "198.51.100.7";
const PROXY_VIP: &str = "192.0.2.250";

struct Fixture {
    core: ProxyCore,
    registry: SimulatedWorkerRegistry,
    metrics: Arc<ProxyMetrics>,
    sbc: Box<dyn UdpEndpoint>,
    workers: Vec<Box<dyn UdpEndpoint>>,
}

/// A proxy over two `Alive` workers whose last advertisement is known, its
/// first view in; `answering` turns its supervision answer on.
async fn proxy(answering: bool) -> Fixture {
    proxy_viewing(answering, true).await
}

/// [`proxy`], its first view in or still to come.
async fn proxy_viewing(answering: bool, view_in: bool) -> Fixture {
    let clock = Clock::test_at(0);
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("w1", ProxyAddr::new(W1, 5060)),
            WorkerEntry::alive("w2", ProxyAddr::new(W2, 5060)),
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
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at: String| BindUdpOpts::new(at.parse().unwrap(), 64);
    let ep = net.bind_udp(bind(format!("{PROXY_VIP}:5060"))).await.unwrap();
    let sbc = net.bind_udp(bind(format!("{SBC}:5060"))).await.unwrap();
    let mut workers = Vec::new();
    for host in [W1, W2] {
        workers.push(net.bind_udp(bind(format!("{host}:5060"))).await.unwrap());
    }
    let capabilities = WorkerCapabilities::unheard();
    capabilities.advertised(&[
        SipHeader { name: "Allow".into(), value: "INVITE, ACK, CANCEL, BYE, OPTIONS".into() },
        SipHeader { name: "Supported".into(), value: "replaces".into() },
        SipHeader { name: "Accept".into(), value: "application/sdp".into() },
    ]);
    let view = FirstView::pending();
    if view_in {
        view.latch();
    }
    let builder = ProxyCoreBuilder::for_test(
        ProxyAddr::new(PROXY_VIP, 5060),
        strategy,
        Arc::new(registry.clone()),
    )
    .clock(clock)
    .metrics(metrics.clone())
    .first_view(view);
    let builder = if answering { builder.supervision(capabilities) } else { builder };
    Fixture { core: builder.build(ep), registry, metrics, sbc, workers }
}

fn options(uri: &str, branch: &str, max_forwards: u32) -> SipMessage {
    let raw = format!(
        "OPTIONS {uri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {SBC}:5060;branch={branch};rport\r\n\
Max-Forwards: {max_forwards}\r\n\
From: <sip:ping@{SBC}>;tag=sbc\r\n\
To: <{uri}>\r\n\
Call-ID: {branch}@{SBC}\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).unwrap()
}

fn sbc() -> SocketAddr {
    format!("{SBC}:5060").parse().unwrap()
}

async fn next(ep: &dyn UdpEndpoint) -> Option<SipMessage> {
    let packet = tokio::time::timeout(Duration::from_millis(200), ep.recv()).await.ok()??;
    CustomParser::new().parse(&packet.raw).ok()
}

async fn relayed(f: &Fixture) -> bool {
    for w in &f.workers {
        if next(w.as_ref()).await.is_some() {
            return true;
        }
    }
    false
}

/// To the VIP, bare, with its port and `transport`, or at `Max-Forwards: 0`
/// (RFC 3261 §16.3): a `200` with a To-tag stating the workers' last
/// advertisement, never relayed.
#[tokio::test(start_paused = true)]
async fn a_supervision_options_is_answered_200_by_the_proxy_with_the_workers_capabilities() {
    let f = proxy(true).await;
    let uris = [
        (format!("sip:{PROXY_VIP}"), 70),
        (format!("sip:{PROXY_VIP}:5060;transport=udp"), 70),
        (format!("sip:{PROXY_VIP}"), 0),
    ];
    for (n, (uri, mf)) in uris.iter().enumerate() {
        f.core.handle_request_from(options(uri, &format!("z9hG4bKsup{n}"), *mf), sbc()).await;
        let Some(SipMessage::Response(resp)) = next(f.sbc.as_ref()).await else {
            panic!("{uri}: no answer");
        };
        assert_eq!(resp.status(), 200, "{uri}");
        assert!(resp.to().tag().is_some(), "{uri}: a To-tag");
        let raw = String::from_utf8_lossy(resp.image()).to_string();
        assert!(raw.contains("Allow: INVITE, ACK, CANCEL, BYE, OPTIONS\r\n"), "{raw}");
        assert!(raw.contains("Supported: replaces\r\n"), "{raw}");
        assert!(raw.contains("Accept: application/sdp\r\n"), "{raw}");
    }
    assert!(!relayed(&f).await, "a supervision OPTIONS reaches no worker");
    assert_eq!(f.metrics.local_options(LocalOptions::Ok), 3);
}

/// With every worker `Slow` the proxy still answers `200`; with none `Alive`
/// or `Slow`, or before the first view, the OPTIONS is dropped unanswered and
/// counted, never answered `503`.
#[tokio::test(start_paused = true)]
async fn a_supervision_options_is_answered_while_a_worker_is_slow_and_dropped_while_none_serves() {
    let f = proxy(true).await;
    let uri = format!("sip:{PROXY_VIP}");
    f.registry.set_health("w1", WorkerHealth::Slow);
    f.registry.set_health("w2", WorkerHealth::Slow);
    f.core.handle_request_from(options(&uri, "z9hG4bKslow", 70), sbc()).await;
    assert!(
        matches!(next(f.sbc.as_ref()).await, Some(SipMessage::Response(r)) if r.status() == 200)
    );

    f.registry.set_health("w1", WorkerHealth::Dead);
    f.registry.set_health("w2", WorkerHealth::Dead);
    f.core.handle_request_from(options(&uri, "z9hG4bKnone", 70), sbc()).await;
    assert!(next(f.sbc.as_ref()).await.is_none(), "no worker serves: unanswered");
    assert!(!relayed(&f).await);
    assert_eq!(f.metrics.local_options(LocalOptions::Dropped), 1);
}

/// Before the first view the OPTIONS is dropped, whatever the workers read.
#[tokio::test(start_paused = true)]
async fn a_supervision_options_before_the_first_view_is_dropped() {
    let f = proxy_viewing(true, false).await;
    f.core
        .handle_request_from(options(&format!("sip:{PROXY_VIP}"), "z9hG4bKboot", 70), sbc())
        .await;
    assert!(next(f.sbc.as_ref()).await.is_none());
    assert_eq!(f.metrics.local_options(LocalOptions::Dropped), 1);
}

/// An OPTIONS with a user part is a request for a party behind the proxy:
/// relayed to a worker, as is every OPTIONS with the answer off.
#[tokio::test(start_paused = true)]
async fn an_options_with_a_user_part_or_with_the_answer_off_is_relayed() {
    let f = proxy(true).await;
    let to_user = options(&format!("sip:alice@{PROXY_VIP}"), "z9hG4bKuser", 70);
    f.core.handle_request_from(to_user, sbc()).await;
    assert!(relayed(&f).await, "an OPTIONS with a user part is relayed");
    assert_eq!(f.metrics.local_options(LocalOptions::Ok), 0);

    let off = proxy(false).await;
    off.core
        .handle_request_from(options(&format!("sip:{PROXY_VIP}"), "z9hG4bKoff", 70), sbc())
        .await;
    assert!(relayed(&off).await, "PROXY_LOCAL_OPTIONS=off relays every OPTIONS");
}
