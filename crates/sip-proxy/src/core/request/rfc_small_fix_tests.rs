//! Small §16.3/§7.3.1 wire contracts: silent ACK discard at Max-Forwards 0,
//! comma-folded Route stripping, out-of-range URI ports staying malformed.

use std::sync::Arc;

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
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "172.20.255.250";
const W1: &str = "10.0.0.1";

async fn core_with_metrics() -> (ProxyCore, Arc<ProxyMetrics>) {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let metrics = Arc::new(ProxyMetrics::new());
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(ep);
    (core, metrics)
}

fn parse_req(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

// §16.3 check 2: an ACK with Max-Forwards: 0 is silently discarded — a 483
// (or any response) to an ACK is a protocol violation.
#[tokio::test]
async fn ack_at_max_forwards_zero_is_dropped_silently() {
    let (core, metrics) = core_with_metrics().await;
    let req = parse_req(&format!(
        "ACK sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKmf0;rport\r\n\
Max-Forwards: 0\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: mf0-1@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let before = metrics.messages_total();
    let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
    assert_eq!(metrics.messages_total(), before, "no response (and no forward) may be generated for the ACK");
}

// §7.3.1: a UA may fold its route set into ONE comma-combined Route header.
// Stripping our own entry must pop only that entry — deleting the whole
// line drops the downstream proxy's Route and bypasses its route set.
#[tokio::test]
async fn folded_route_strip_preserves_the_downstream_route() {
    let (core, _metrics) = core_with_metrics().await;
    let req = parse_req(&format!(
        "BYE sip:b2bua@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKfold;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: fold-1@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;lr>, <sip:10.9.9.9:5062;lr>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
    assert_eq!(
        outcome.decision,
        RoutingDecisionKind::LooseRoute,
        "the surviving downstream Route entry must drive loose routing"
    );
    assert_eq!(outcome.target, Some(ProxyAddr::new("10.9.9.9", 5062)));
}

// An out-of-range URI port (70596 & 0xFFFF == 5060) must read as malformed,
// never silently truncate into an alias of the proxy's own address.
#[tokio::test]
async fn oversized_route_port_does_not_alias_the_advertised_address() {
    let (core, _metrics) = core_with_metrics().await;
    let req = parse_req(&format!(
        "BYE sip:b2bua@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKbig;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: bigport-1@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:70596;lr>\r\n\
Content-Length: 0\r\n\r\n"
    ));
    let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
    // A 70596→5060 wrap would match the advertised VIP and strip the entry
    // as a self-route. It must stay foreign (and being malformed, it can't
    // drive loose routing either) → plain selection.
    assert_eq!(outcome.decision, RoutingDecisionKind::SelectNew);
}
