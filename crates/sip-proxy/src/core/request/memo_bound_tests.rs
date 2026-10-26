//! A transaction's CANCEL and non-2xx ACK keep finding their hop while a
//! flood of out-of-dialog requests holds the branch memo at its cap: the
//! flood's entries are the ones the cap forgets.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::SignalingNetwork;
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::addr::ProxyAddr;
use crate::cancel_lru::{CancelBranchLru, MemoBounds};
use crate::core::{ProxyCore, ProxyCoreBuilder};
use crate::observability::metrics::{CancelLookup, RoutingDecisionKind};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::registry::WorkerRegistry;
use crate::strategies::forward_all::ForwardAllStrategy;
use crate::{ProxyMetrics, RoutingStrategy};

const UAC: &str = "10.244.7.13";
const FLOODER: &str = "203.0.113.66";
const PROXY_VIP: &str = "192.0.2.250";
const W1: &str = "10.0.0.1";
const W2: &str = "10.0.0.2";
const CAP: usize = 16;

async fn core(lru: Arc<CancelBranchLru>, metrics: Arc<ProxyMetrics>) -> ProxyCore {
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
        .cancel_lru(lru)
        .metrics(metrics)
        .build(ep)
}

fn parse(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn uac_request(method: &str, cseq_method: &str, to_tag: &str) -> SipMessage {
    parse(&format!(
        "{method} sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>{to_tag}\r\n\
Call-ID: bounded-1@test\r\n\
CSeq: 1 {cseq_method}\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// One out-of-dialog OPTIONS of a flood, each its own transaction.
fn flood_options(i: usize) -> SipMessage {
    parse(&format!(
        "OPTIONS sip:probe@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER}:5060;branch=z9hG4bKflood{i};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:flood@{FLOODER}>;tag=f{i}\r\n\
To: <sip:probe@10.0.0.50>\r\n\
Call-ID: flood-{i}@test\r\n\
CSeq: 1 OPTIONS\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The callee's 486 to the INVITE, relayed by the proxy.
fn busy_486() -> sip_message::SipResponse {
    let raw = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKinv;rport\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: bounded-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
        panic!("expected a response")
    };
    resp
}

async fn flood(core: &ProxyCore, from: usize, count: usize) {
    let src: SocketAddr = format!("{FLOODER}:5060").parse().unwrap();
    for i in from..from + count {
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        core.route_request_from(&flood_options(i), src).await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_and_ack_find_their_hop_through_a_flood_at_the_cap() {
    let lru = Arc::new(CancelBranchLru::with_bounds(
        Clock::test_at(0),
        MemoBounds { max_entries: CAP, max_bytes: usize::MAX, stripes: 1 },
    ));
    let metrics = Arc::new(ProxyMetrics::new());
    let core = core(lru.clone(), metrics.clone()).await;
    let uac: SocketAddr = format!("{UAC}:5060").parse().unwrap();

    core.route_request_from(&uac_request("INVITE", "INVITE", ""), uac).await;
    flood(&core, 0, 3 * CAP).await;
    assert!(lru.size() <= CAP, "the flood holds the memo at its cap");
    assert!(lru.evicted() > 0, "the flood's own memos are evicted");

    let cancel = core.route_request_from(&uac_request("CANCEL", "CANCEL", ""), uac).await;
    assert_eq!(cancel.decision, RoutingDecisionKind::Cancel);
    assert_eq!(cancel.target, Some(ProxyAddr::new(W1, 5060)));
    assert_eq!(
        metrics.cancel_lookup_count(CancelLookup::Hit),
        1,
        "the CANCEL follows its INVITE's remembered hop"
    );

    // The 486 comes from the node that took the transaction over.
    let survivor: SocketAddr = format!("{W2}:5060").parse().unwrap();
    core.handle_response_from(busy_486(), survivor).await;
    flood(&core, 3 * CAP, 2).await;
    let ack = core.route_request_from(&uac_request("ACK", "ACK", ";tag=callee-1"), uac).await;
    assert_eq!(ack.decision, RoutingDecisionKind::AckHop);
    assert_eq!(ack.target, Some(ProxyAddr::new(W2, 5060)), "the ACK reaches the final's sender");
}

/// One INVITE of a flood carrying a To-tag and no Route: no admission gate
/// meets it, and each writes an INVITE entry beside its retransmission memo.
fn flood_tagged_invite(i: usize) -> SipMessage {
    parse(&format!(
        "INVITE sip:probe@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER}:5060;branch=z9hG4bKtagged{i};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:flood@{FLOODER}>;tag=f{i}\r\n\
To: <sip:probe@10.0.0.50>;tag=t{i}\r\n\
Call-ID: tagged-{i}@test\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:flood@{FLOODER}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// A flood that fills the memo with INVITE entries, which outlive every
/// short memo, leaves a later INVITE its retransmission memo and a relayed
/// non-2xx final its ACK hop: the short memos are not the ones a long-lived
/// flood pushes out.
#[tokio::test(start_paused = true)]
async fn a_flood_of_invite_entries_keeps_the_short_memos_of_a_new_transaction() {
    let lru = Arc::new(CancelBranchLru::with_bounds(
        Clock::test_at(0),
        MemoBounds { max_entries: CAP, max_bytes: usize::MAX, stripes: 1 },
    ));
    let metrics = Arc::new(ProxyMetrics::new());
    let core = core(lru.clone(), metrics.clone()).await;
    let flooder: SocketAddr = format!("{FLOODER}:5060").parse().unwrap();
    for i in 0..2 * CAP {
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        core.route_request_from(&flood_tagged_invite(i), flooder).await;
    }
    tokio::time::advance(std::time::Duration::from_millis(1)).await;

    let uac: SocketAddr = format!("{UAC}:5060").parse().unwrap();
    let invite = uac_request("INVITE", "INVITE", "");
    core.route_request_from(&invite, uac).await;
    let SipMessage::Request(req) = &invite else { unreachable!() };
    let rtx = crate::cancel_lru::retransmit_key(
        req.top_via(),
        req.call_id().as_str(),
        req.from().tag(),
        "INVITE",
        1,
    );
    assert!(lru.lookup(&rtx).is_some(), "the INVITE's retransmission memo is kept");

    let survivor: SocketAddr = format!("{W2}:5060").parse().unwrap();
    core.handle_response_from(busy_486(), survivor).await;
    let ack = core.route_request_from(&uac_request("ACK", "ACK", ";tag=callee-1"), uac).await;
    assert_eq!(ack.decision, RoutingDecisionKind::AckHop, "the relayed final's ACK hop is kept");
    assert_eq!(ack.target, Some(ProxyAddr::new(W2, 5060)));
    assert!(lru.lookup(&rtx).is_some(), "and the retransmission memo still");
}
