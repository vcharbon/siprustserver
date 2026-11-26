//! The non-2xx ACK hop decision is keyed on the §17.1.1.3 branch identity (an
//! ACK for a non-2xx final reuses its INVITE's top-Via branch), not on the
//! absence of a Route header: a b-leg INVITE behind an outbound proxy carries a
//! preloaded Route, and so does its non-2xx ACK.
//! A caller's matched ACK for a relayed final is relayed to the node the final
//! arrived from, on the INVITE's outbound branch, so the downstream server
//! transaction matches it and stops retransmitting the final; a matched ACK for
//! a final the proxy generated itself is absorbed (the proxy is the UAS).

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::observability::metrics::RoutingDecisionKind;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::RoutingStrategy;

const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "192.0.2.250";
const W1: &str = "10.0.0.1";
const W2: &str = "10.0.0.2";

async fn core() -> ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    ProxyCoreBuilder::for_test(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .build(ep)
}

fn parse_req(raw: &str) -> SipMessage {
    CustomParser::new().parse(raw.as_bytes()).unwrap()
}

fn invite(branch: &str) -> SipMessage {
    parse_req(&format!(
        "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: absorb-1@test\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Route: <sip:{PROXY_VIP}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn ack(branch: &str, route: &str) -> SipMessage {
    parse_req(&format!(
        "ACK sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: absorb-1@test\r\n\
CSeq: 1 ACK\r\n\
{route}Content-Length: 0\r\n\r\n"
    ))
}

fn src() -> std::net::SocketAddr {
    format!("{UAC}:5060").parse().unwrap()
}

/// The node a final arrives from — in steady state the INVITE's target.
fn worker_src() -> std::net::SocketAddr {
    format!("{W1}:5060").parse().unwrap()
}

/// The non-2xx final coming back to the proxy: top Via = the proxy (so it
/// relays it and writes the `ackhop|` relay memo), second Via = the
/// upstream's INVITE Via (its branch is what the upstream's own §17.1.1.3
/// ACK will carry).
fn busy_486(upstream_branch: &str) -> sip_message::types::SipResponse {
    let raw = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={upstream_branch};rport\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: absorb-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("expected response")
    };
    resp
}

/// The non-2xx ACK carries the INVITE's preloaded outbound-proxy Route
/// (§17.1.1.3 copies the Route verbatim). The memo and the branch identity
/// classify it, and it is relayed on the INVITE's exact hop (`AckHop`), never
/// re-run through the strategy under a fresh branch.
#[tokio::test]
async fn non_2xx_ack_with_preloaded_proxy_route_follows_the_invite_hop() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;
    core.handle_response_from(busy_486("z9hG4bKinv"), worker_src()).await;

    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let outcome = core.route_request_from(&ack("z9hG4bKinv", &route), src()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::AckHop);
    assert_eq!(
        outcome.target,
        Some(ProxyAddr::new(W1, 5060)),
        "the same-branch (non-2xx) ACK must be relayed on the INVITE's hop"
    );
}

#[tokio::test]
async fn non_2xx_ack_without_route_follows_the_invite_hop() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;
    core.handle_response_from(busy_486("z9hG4bKinv"), worker_src()).await;

    let outcome = core.route_request_from(&ack("z9hG4bKinv", ""), src()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::AckHop);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1, 5060)));
}

/// The hop memo names the node the final arrived from, not the node the
/// INVITE was forwarded to: after a failover the survivor answers on the
/// INVITE's outbound branch, and only the survivor holds the server
/// transaction the caller's ACK must quench (§17.1.1.3).
#[tokio::test]
async fn non_2xx_ack_follows_the_finals_sender_not_the_invite_target() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;
    let survivor: std::net::SocketAddr = format!("{W2}:5060").parse().unwrap();
    core.handle_response_from(busy_486("z9hG4bKinv"), survivor).await;

    let outcome = core.route_request_from(&ack("z9hG4bKinv", ""), src()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::AckHop);
    assert_eq!(
        outcome.target,
        Some(ProxyAddr::new(W2, 5060)),
        "the ACK must reach the node that sent the final, not the INVITE's stale target"
    );
}

/// A fresh-branch (2xx) ACK arriving after a relayed non-2xx final for the
/// same (Call-ID, From-tag, CSeq) is a different transaction (§13.2.2.4): the
/// memo's branch does not match, so it takes the normal routing ladder.
#[tokio::test]
async fn fresh_branch_ack_after_a_relayed_final_takes_the_normal_ladder() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;
    core.handle_response_from(busy_486("z9hG4bKinv"), worker_src()).await;

    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let outcome = core.route_request_from(&ack("z9hG4bKack2xx", &route), src()).await;
    assert!(outcome.target.is_some(), "a fresh-branch ACK must still be forwarded");
    assert_ne!(
        outcome.decision,
        RoutingDecisionKind::AckHop,
        "a fresh-branch ACK is not the hop ACK"
    );
}

/// An ACK for a 2xx is its own transaction (§13.2.2.4), forwarded end-to-end by
/// the normal ladder even when its fresh branch aliases the INVITE's: with no
/// non-2xx final relayed there is no `ackhop|` memo.
#[tokio::test]
async fn ack_for_2xx_is_forwarded_even_when_its_branch_aliases_the_invite() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;

    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let outcome = core.route_request_from(&ack("z9hG4bKinv", &route), src()).await;
    assert!(
        outcome.target.is_some(),
        "with no relayed non-2xx final, an ACK must be forwarded even on a branch alias"
    );
}

#[tokio::test]
async fn ack_for_2xx_with_fresh_branch_is_forwarded() {
    let core = core().await;
    core.route_request_from(&invite("z9hG4bKinv"), src()).await;

    let route = format!("Route: <sip:{PROXY_VIP}:5060;lr>\r\n");
    let outcome = core.route_request_from(&ack("z9hG4bKack2xx", &route), src()).await;
    assert!(outcome.target.is_some(), "a fresh-branch (2xx) ACK must be forwarded end-to-end");
}

/// §16.7 / §17.1.1.3: a final the proxy generated itself (here a 420 to an
/// unsupported Proxy-Require) makes it the UAS of the INVITE transaction, so
/// the upstream's same-branch ACK terminates at this hop: it is absorbed, never
/// run through the strategy and handed to a worker as a stray ACK.
#[tokio::test]
async fn ack_for_a_self_generated_reject_is_absorbed() {
    let core = core().await;
    let inv = parse_req(&format!(
        "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: absorb-1@test\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Proxy-Require: bogus-extension-xyz\r\n\
Content-Length: 0\r\n\r\n"
    ));
    core.route_request_from(&inv, src()).await;
    let SipMessage::Request(req) = &inv else { unreachable!() };
    let to_tag = core.refusals.to_tag(req);
    let ack = parse_req(&format!(
        "ACK sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag={to_tag}\r\n\
Call-ID: absorb-1@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    ));

    let outcome = core.route_request_from(&ack, src()).await;
    assert_eq!(
        outcome.target, None,
        "the ACK to a self-generated non-2xx final must be absorbed at this hop"
    );
}
