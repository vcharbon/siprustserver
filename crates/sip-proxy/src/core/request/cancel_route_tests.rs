//! A CANCEL and a non-2xx ACK follow their own INVITE transaction (RFC 3261
//! §9.1, §16.10, §17.1.1.3): the top Via branch and sent-by the INVITE arrived
//! with (§17.2.3). In a spiral (§16.3) a request leaves through this proxy and
//! comes back through it with its Call-ID, From tag and CSeq unchanged, on a
//! new branch from another sender: two INVITE transactions under one dialog
//! identity, each with its own downstream.

use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipResponse;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

use crate::addr::ProxyAddr;
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::observability::metrics::RoutingDecisionKind;
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::{WorkerEntry, WorkerRegistry};
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::RoutingStrategy;

/// The worker behind this proxy: a registered sent-by, and where a new
/// request from outside is forwarded.
const WORKER: &str = "10.244.5.8";
/// A proxy downstream of this one that sends the worker's request back.
const THIRD: &str = "10.0.0.60";
const PROXY_VIP: &str = "192.0.2.250";

const CALL_ID: &str = "spiral-1@test";
const WORKER_BRANCH: &str = "z9hG4bKworker1";
const THIRD_BRANCH: &str = "z9hG4bKthird1";

async fn core() -> ProxyCore {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net
        .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
        .await
        .unwrap();
    let reg: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w1",
            ProxyAddr::new(WORKER, 5060),
        )]));
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new(WORKER, 5060)));
    ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
        .clock(Clock::test_at(0))
        .build(ep)
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn response(raw: &str) -> SipResponse {
    let SipMessage::Response(resp) = parse(raw) else { panic!("a response") };
    resp
}

fn worker_src() -> std::net::SocketAddr {
    format!("{WORKER}:5060").parse().unwrap()
}

fn third_src() -> std::net::SocketAddr {
    format!("{THIRD}:5060").parse().unwrap()
}

/// The dialog identity both transactions share.
fn identity(method: &str) -> String {
    format!(
        "From: <sip:alice@{WORKER}>;tag=tag-a\r\n\
To: <sip:bob@{THIRD}>\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 {method}\r\n"
    )
}

/// The worker's INVITE toward the downstream proxy: its R-URI names it.
fn worker_invite() -> SipMessage {
    parse(&format!(
        "INVITE sip:bob@{THIRD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={WORKER_BRANCH};rport\r\n\
Max-Forwards: 70\r\n\
{}Contact: <sip:alice@{WORKER}:5060>\r\n\
Content-Length: 0\r\n\r\n",
        identity("INVITE")
    ))
}

/// The same request spiralled back by the downstream proxy (§16.3): its own
/// Via on top, a new branch, Max-Forwards lower, the identity unchanged.
fn spiralled_invite() -> SipMessage {
    parse(&format!(
        "INVITE sip:bob@{PROXY_VIP}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {THIRD}:5060;branch={THIRD_BRANCH};rport\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKproxyhop;rport\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={WORKER_BRANCH};rport;received={WORKER}\r\n\
Max-Forwards: 68\r\n\
{}Contact: <sip:alice@{WORKER}:5060>\r\n\
Content-Length: 0\r\n\r\n",
        identity("INVITE")
    ))
}

/// The worker's CANCEL of its INVITE (§9.1: the INVITE's top Via).
fn worker_cancel() -> SipMessage {
    parse(&format!(
        "CANCEL sip:bob@{THIRD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={WORKER_BRANCH};rport\r\n\
Max-Forwards: 70\r\n\
{}Content-Length: 0\r\n\r\n",
        identity("CANCEL")
    ))
}

/// The downstream proxy's CANCEL of the spiralled INVITE (§16.10: its own
/// client transaction's Via).
fn third_cancel() -> SipMessage {
    parse(&format!(
        "CANCEL sip:bob@{PROXY_VIP}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {THIRD}:5060;branch={THIRD_BRANCH};rport\r\n\
Max-Forwards: 70\r\n\
{}Content-Length: 0\r\n\r\n",
        identity("CANCEL")
    ))
}

fn ack(sender: &str, branch: &str, ruri_host: &str) -> SipMessage {
    parse(&format!(
        "ACK sip:bob@{ruri_host}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {sender}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{WORKER}>;tag=tag-a\r\n\
To: <sip:bob@{THIRD}>;tag=callee-1\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// A 487 coming back to this proxy for the INVITE the Via stack below its own
/// entry names.
fn terminated(upstream_vias: &str) -> SipResponse {
    response(&format!(
        "SIP/2.0 487 Request Terminated\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
{upstream_vias}From: <sip:alice@{WORKER}>;tag=tag-a\r\n\
To: <sip:bob@{THIRD}>;tag=callee-1\r\n\
Call-ID: {CALL_ID}\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

#[derive(Clone, Copy, Debug)]
enum Order {
    /// The spiral: the worker's INVITE leaves, then comes back.
    WorkerFirst,
    /// The same two transactions arriving the other way round.
    ThirdFirst,
}

async fn both_invites(core: &ProxyCore, order: Order) {
    let worker = || async {
        let out = core.route_request(&worker_invite(), worker_src()).await;
        assert_eq!(out.target, Some(ProxyAddr::new(THIRD, 5060)), "the worker's INVITE leaves");
    };
    let third = || async {
        let out = core.route_request(&spiralled_invite(), third_src()).await;
        assert_eq!(out.target, Some(ProxyAddr::new(WORKER, 5060)), "the spiral comes back");
    };
    match order {
        Order::WorkerFirst => {
            worker().await;
            third().await;
        }
        Order::ThirdFirst => {
            third().await;
            worker().await;
        }
    }
}

async fn each_cancel_follows_its_own_invite(order: Order) {
    let core = core().await;
    both_invites(&core, order).await;

    let out = core.route_request(&worker_cancel(), worker_src()).await;
    assert_eq!(out.decision, RoutingDecisionKind::Cancel);
    assert_eq!(
        out.target,
        Some(ProxyAddr::new(THIRD, 5060)),
        "{order:?}: the worker's CANCEL goes where its INVITE went, the downstream proxy"
    );

    let out = core.route_request(&third_cancel(), third_src()).await;
    assert_eq!(out.decision, RoutingDecisionKind::Cancel);
    assert_eq!(
        out.target,
        Some(ProxyAddr::new(WORKER, 5060)),
        "{order:?}: the downstream proxy's CANCEL goes where the spiralled INVITE went"
    );
}

#[tokio::test(start_paused = true)]
async fn a_spirals_two_cancels_each_follow_their_own_invite() {
    each_cancel_follows_its_own_invite(Order::WorkerFirst).await;
}

#[tokio::test(start_paused = true)]
async fn two_invites_on_one_identity_keep_their_cancels_apart_in_either_order() {
    each_cancel_follows_its_own_invite(Order::ThirdFirst).await;
}

// Both INVITEs end on a 487 relayed through this proxy, the spiralled one's
// first. Each upstream's §17.1.1.3 ACK reaches the node its own final came
// from: the 487 relayed second must not take the first one's ACK hop.
#[tokio::test(start_paused = true)]
async fn a_spirals_two_non_2xx_acks_each_follow_their_own_final() {
    let core = core().await;
    both_invites(&core, Order::WorkerFirst).await;

    let spiralled_vias = format!(
        "Via: SIP/2.0/UDP {THIRD}:5060;branch={THIRD_BRANCH};rport\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKproxyhop;rport\r\n\
Via: SIP/2.0/UDP {WORKER}:5060;branch={WORKER_BRANCH};rport\r\n"
    );
    core.handle_response(terminated(&spiralled_vias), worker_src()).await;
    let worker_vias = format!("Via: SIP/2.0/UDP {WORKER}:5060;branch={WORKER_BRANCH};rport\r\n");
    core.handle_response(terminated(&worker_vias), third_src()).await;

    let out = core.route_request(&ack(THIRD, THIRD_BRANCH, PROXY_VIP), third_src()).await;
    assert_eq!(out.decision, RoutingDecisionKind::AckHop, "the spiralled INVITE's ACK");
    assert_eq!(out.target, Some(ProxyAddr::new(WORKER, 5060)), "to where its 487 came from");

    let out = core.route_request(&ack(WORKER, WORKER_BRANCH, THIRD), worker_src()).await;
    assert_eq!(out.decision, RoutingDecisionKind::AckHop, "the worker's INVITE's ACK");
    assert_eq!(out.target, Some(ProxyAddr::new(THIRD, 5060)), "to where its 487 came from");
}
