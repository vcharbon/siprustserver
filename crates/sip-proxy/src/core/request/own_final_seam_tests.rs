//! What the proxy's one own-final seam reports names where the final went: the
//! `sip.out` of a traced call and the peer-failure metric, on a peer whose source
//! port and sent-by port differ.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use observe::{activation_bucket, RateDraw, SampleAdmission};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{SendError, SendErrorKind, UdpEndpoint, UdpEndpointCounters, UdpPacket};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::self_gate::{AdmitDecision, ProxySelfGate};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::trace::ProxyTraces;
use crate::{ProxyMetrics, RoutingStrategy};

const SOURCE: &str = "192.0.2.20:40000";
const SENT_BY: &str = "192.0.2.20:5070";

/// A socket whose every send fails.
struct Refusing;

#[async_trait]
impl UdpEndpoint for Refusing {
    async fn send_to(&self, _buf: &[u8], _dst: SocketAddr) -> Result<(), SendError> {
        Err(SendError { kind: SendErrorKind::Unreachable, message: "refused".into() })
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        "192.0.2.250:5060".parse().expect("fixture address")
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

/// A gate that sheds every external new-dialog INVITE.
struct Shedding;

impl ProxySelfGate for Shedding {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision::Reject {
            reason: crate::self_gate::ShedReason::IntakeAge,
            retry_after_sec: 7,
        }
    }
}

fn core(metrics: Arc<ProxyMetrics>) -> ProxyCore {
    let worker = ProxyAddr::new("10.0.0.9", 5060);
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(worker));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let traces = Arc::new(ProxyTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), activation_bucket(0)),
        false,
    ));
    ProxyCoreBuilder::for_test(ProxyAddr::new("192.0.2.250", 5060), strategy, registry)
        .clock(Clock::test_at(0))
        .traces(traces)
        .metrics(metrics)
        .self_gate(Arc::new(Shedding))
        .build(Box::new(Refusing))
}

fn invite() -> SipMessage {
    let raw = format!(
        "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {SENT_BY};branch=z9hG4bK-seam\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.20>;tag=a\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: seam@192.0.2.20\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{SENT_BY}>\r\n\
Content-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(raw.as_bytes()).expect("an INVITE")
}

#[tokio::test]
async fn the_trace_and_the_peer_failure_name_the_finals_destination_not_the_source() {
    let (_guard, log) = observe_testkit::test_buffer();
    let metrics = Arc::new(ProxyMetrics::new());
    let core = core(metrics.clone());

    core.handle_request_from(invite(), SOURCE.parse().expect("an address")).await;

    let out = log.matching("kind=sip.out");
    assert_eq!(out.len(), 1, "the 503 is traced");
    assert!(out[0].contains(&format!("to {SENT_BY}")), "{}", out[0].line());
    assert!(!out[0].contains(SOURCE), "{}", out[0].line());

    let text = metrics.prometheus_text();
    assert!(text.contains(&format!("peer=\"{SENT_BY}\"")), "{text}");
    assert!(!text.contains(&format!("peer=\"{SOURCE}\"")), "{text}");
}
