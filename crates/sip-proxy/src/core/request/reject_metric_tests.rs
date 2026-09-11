//! Reject-reason attribution: every locally decided reject increments
//! `sip_proxy_rejects_total{reason}`, so a proxy-generated 503 (and its
//! siblings) is attributable from metrics alone — the aggregate
//! `sip_routing_decision_total{kind="reject"}` merges all causes.

use std::sync::Arc;

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
use crate::registry::WorkerRegistry;
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
use crate::ProxyMetrics;

const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "172.20.255.250";

/// Strategy double whose selection always fails with the given error — drives
/// `reply_select_failure` through the public routing path.
struct FailingSelect(fn() -> SelectError);

#[async_trait]
impl RoutingStrategy for FailingSelect {
    fn name(&self) -> &str {
        "FailingSelect"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err((self.0)())
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
}

async fn core_failing_with(err: fn() -> SelectError) -> (ProxyCore, Arc<ProxyMetrics>) {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(FailingSelect(err));
    let metrics = Arc::new(ProxyMetrics::new());
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(ep);
    (core, metrics)
}

fn new_dialog_invite() -> SipMessage {
    let raw = format!(
        "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKrej1;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: rej-1@test\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

// SelectError::NoTarget → 503 with Reason text "no_target_available"; the
// reject counter carries the same reason.
#[tokio::test]
async fn no_target_select_failure_attributes_its_reject() {
    let (core, metrics) =
        core_failing_with(|| SelectError::NoTarget { reason: "empty registry".into() }).await;
    let outcome =
        core.route_request(&new_dialog_invite(), format!("{UAC}:5060").parse().unwrap()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
    assert_eq!(metrics.reject_count("no_target_available"), 1);
    assert_eq!(metrics.reject_count("worker_rate_capped"), 0);
    assert!(metrics
        .prometheus_text()
        .contains("sip_proxy_rejects_total{reason=\"no_target_available\"} 1"));
}

// SelectError::RateCapExhausted → 503 with Reason text "worker_rate_capped";
// the reject counter splits it from the outage bucket.
#[tokio::test]
async fn rate_cap_select_failure_attributes_its_reject() {
    let (core, metrics) = core_failing_with(|| SelectError::RateCapExhausted {
        worker_id: "w1".into(),
        retry_after_sec: 1,
    })
    .await;
    let outcome =
        core.route_request(&new_dialog_invite(), format!("{UAC}:5060").parse().unwrap()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
    assert_eq!(metrics.reject_count("worker_rate_capped"), 1);
    assert_eq!(metrics.reject_count("no_target_available"), 0);
    assert!(metrics
        .prometheus_text()
        .contains("sip_proxy_rejects_total{reason=\"worker_rate_capped\"} 1"));
}

// A non-ACK request at Max-Forwards 0 is answered 483 — and attributed.
#[tokio::test]
async fn too_many_hops_attributes_its_reject() {
    let (core, metrics) =
        core_failing_with(|| SelectError::NoTarget { reason: "unused".into() }).await;
    let raw = format!(
        "BYE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKrej2;rport\r\n\
Max-Forwards: 0\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: rej-2@test\r\n\
CSeq: 2 BYE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let req = CustomParser::default().parse(raw.as_bytes()).unwrap();
    let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
    assert_eq!(metrics.reject_count("too_many_hops"), 1);
}
