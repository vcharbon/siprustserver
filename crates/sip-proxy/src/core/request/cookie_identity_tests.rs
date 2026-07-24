//! The stickiness-cookie identity for worker-originated dialog-creating
//! requests: encoded for the worker's SNAT-immune registry identity (top-Via
//! sent-by), with the UDP source as the pod-direct fallback.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::observability::metrics::RoutingDecisionKind;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

const W1_POD: &str = "10.244.5.8";
const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "172.20.255.250";
const SNAT_NODE: &str = "172.20.0.11";

/// Strategy double that records the address handed to `encode_stickiness`
/// — the contract under test: for a worker-originated dialog-creating
/// request it must be the worker's REGISTRY identity, not the UDP source.
#[derive(Default)]
struct CookieCaptureStrategy {
    encoded_for: Mutex<Option<ProxyAddr>>,
}

#[async_trait]
impl RoutingStrategy for CookieCaptureStrategy {
    fn name(&self) -> &str {
        "CookieCapture"
    }
    async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
        Err(SelectError::NoTarget { reason: "worker-outbound tests never select".into() })
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        *self.encoded_for.lock().unwrap() = Some(target.clone());
        None
    }
}

struct Fixture {
    core: ProxyCore,
    strategy: Arc<CookieCaptureStrategy>,
}

async fn fixture() -> Fixture {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
    let strategy = Arc::new(CookieCaptureStrategy::default());
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy.clone(), reg)
        .clock(Clock::test_at(0))
        .build(ep);
    Fixture { core, strategy }
}

/// A b-leg INVITE: worker-originated (top Via = the worker), dialog-creating
/// (no To-tag), R-URI = the callee.
fn bleg_invite(top_via_host: &str) -> SipMessage {
    let raw = format!(
        "INVITE sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {top_via_host}:5060;branch=z9hG4bKbleg;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>\r\n\
Call-ID: bleg-1@{UAC}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{top_via_host}:5060;leg=b>\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

// Regression for the b-leg long-call-loss class: behind the keepalived VIP
// the worker's INVITE arrives with src = SNAT node IP + ephemeral port. The
// cookie must be encoded for the worker's REGISTRY identity (the SNAT-immune
// top-Via sent-by) — encoding it for `src` makes encode_stickiness miss the
// registry and emit a param-less cookie RR, so the callee's later in-dialog
// requests decode Unknown and are re-sharded to an arbitrary worker.
#[tokio::test]
async fn snat_masqueraded_bleg_invite_encodes_cookie_for_the_via_worker() {
    let f = fixture().await;
    let req = bleg_invite(W1_POD);
    let snat_src = format!("{SNAT_NODE}:63522").parse().unwrap();

    let outcome = f.core.route_request(&req, snat_src).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(
        *f.strategy.encoded_for.lock().unwrap(),
        Some(ProxyAddr::new(W1_POD, 5060)),
        "cookie must carry the worker's registry identity, not the SNAT source"
    );
}

// Pod-direct fallback: the top Via names an unregistered host (e.g. a
// just-rebooted pod) but the UDP source IS the registered worker — the
// cookie falls back to the source identity.
#[tokio::test]
async fn pod_direct_source_remains_the_cookie_fallback() {
    let f = fixture().await;
    let req = bleg_invite("10.244.9.99"); // not in the registry
    let pod_src = format!("{W1_POD}:5060").parse().unwrap();

    let outcome = f.core.route_request(&req, pod_src).await;

    assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
    assert_eq!(
        *f.strategy.encoded_for.lock().unwrap(),
        Some(ProxyAddr::new(W1_POD, 5060)),
        "un-NAT'd worker source still identifies the cookie"
    );
}
