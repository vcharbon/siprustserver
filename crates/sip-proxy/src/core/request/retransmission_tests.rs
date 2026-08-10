//! The §16.6/§17.2.3 retransmission memo: a re-sent request repeats the
//! original forward (same target, same outbound branch), bypasses the
//! admission gate, and is not re-counted; CANCEL/ACK follow the INVITE's
//! remembered hop for the whole ringing window.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use crate::self_gate::{AdmitDecision, ProxySelfGate};
use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
use crate::ProxyMetrics;

const UAC: &str = "10.244.7.13";
const PROXY_VIP: &str = "172.20.255.250";
const W1: &str = "10.0.0.1";
const W2: &str = "10.0.0.2";

/// Strategy double: pops targets off a queue, counting selections — a
/// changed candidate set is modeled as "the next selection differs".
struct QueueStrategy {
    targets: Mutex<VecDeque<ProxyAddr>>,
    calls: AtomicU32,
}

impl QueueStrategy {
    fn of(targets: &[ProxyAddr]) -> Arc<Self> {
        Arc::new(Self { targets: Mutex::new(targets.iter().cloned().collect()), calls: AtomicU32::new(0) })
    }
}

#[async_trait]
impl RoutingStrategy for QueueStrategy {
    fn name(&self) -> &str {
        "Queue"
    }
    async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.targets.lock().unwrap().pop_front().expect("selection queue exhausted"))
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
}

/// Gate double: admits the first external INVITE, rejects everything after
/// — the shape of a capacity gate that filled up between two transmissions.
#[derive(Default)]
struct AdmitOnceGate {
    used: AtomicBool,
    tries: AtomicU32,
}

impl ProxySelfGate for AdmitOnceGate {
    fn try_admit_external(&self) -> AdmitDecision {
        self.tries.fetch_add(1, Ordering::SeqCst);
        if self.used.swap(true, Ordering::SeqCst) {
            AdmitDecision { admit: false, reason: Some("proxy_overload_cps".into()), retry_after_sec: 3 }
        } else {
            AdmitDecision::admit()
        }
    }
}

struct Fixture {
    core: ProxyCore,
    strategy: Arc<QueueStrategy>,
    gate: Arc<AdmitOnceGate>,
    metrics: Arc<ProxyMetrics>,
}

async fn fixture(targets: &[ProxyAddr]) -> Fixture {
    let net = SimulatedSignalingNetwork::new(1);
    let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
    let strategy = QueueStrategy::of(targets);
    let gate = Arc::new(AdmitOnceGate::default());
    let metrics = Arc::new(ProxyMetrics::new());
    let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy.clone(), reg)
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .self_gate(gate.clone())
        .build(ep);
    Fixture { core, strategy, gate, metrics }
}

fn parse_req(raw: &str) -> SipMessage {
    CustomParser::default().parse(raw.as_bytes()).unwrap()
}

fn invite(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
    parse_req(&format!(
        "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag={from_tag}\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn cancel(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
    parse_req(&format!(
        "CANCEL sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag={from_tag}\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} CANCEL\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

/// The upstream's §17.1.1.3 ACK for a non-2xx final: same branch as its
/// INVITE (same transaction), To-tag echoed from the final.
fn ack(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
    parse_req(&format!(
        "ACK sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag={from_tag}\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} ACK\r\n\
Content-Length: 0\r\n\r\n"
    ))
}

fn src() -> std::net::SocketAddr {
    format!("{UAC}:5060").parse().unwrap()
}

// Guards: an INVITE retransmission that re-runs the strategy while reusing
// the memoized branch would, under a changed candidate set, send the same
// branch to a DIFFERENT worker — one transaction split into two B2BUA
// calls. The retransmit must repeat the original forward.
#[tokio::test(start_paused = true)]
async fn retransmitted_invite_repeats_the_original_selection() {
    let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
    let req = invite("split-1@test", "tag-a", 1, "z9hG4bKr1");

    let first = f.core.route_request(&req, src()).await;
    let retx = f.core.route_request(&req, src()).await;

    assert_eq!(first.target, Some(ProxyAddr::new(W1, 5060)));
    assert_eq!(retx.target, Some(ProxyAddr::new(W1, 5060)), "retransmit must NOT be re-routed to w2");
    assert_eq!(f.strategy.calls.load(Ordering::SeqCst), 1, "the strategy must not run again for a retransmit");
}

// A retransmit of an admitted INVITE must bypass the gate (no 503 to a
// setup already ringing downstream) and not inflate sip_proxy_calls_total.
#[tokio::test(start_paused = true)]
async fn retransmitted_invite_is_not_regated_or_recounted() {
    let f = fixture(&[ProxyAddr::new(W1, 5060)]).await;
    let req = invite("regate-1@test", "tag-a", 1, "z9hG4bKr2");

    let first = f.core.route_request(&req, src()).await;
    let retx = f.core.route_request(&req, src()).await;

    assert_eq!(first.decision, RoutingDecisionKind::SelectNew);
    assert_ne!(retx.decision, RoutingDecisionKind::Reject, "gate must not 503 a retransmission");
    assert_eq!(retx.target, Some(ProxyAddr::new(W1, 5060)));
    assert_eq!(f.gate.tries.load(Ordering::SeqCst), 1, "gate consulted once per transaction");
    assert_eq!(f.metrics.calls_total(), 1, "one call, not one per transmission");
}

// Guards the entry TTL: it must cover the legal ringing window (B2BUA
// SetupTimeout / the default sip-txn initial-INVITE bound, 158 s) — a TTL
// below it forwards a CANCEL after half a minute of ringing via fresh
// selection with a fresh branch → downstream 481 and the callee keeps ringing.
#[tokio::test(start_paused = true)]
async fn cancel_after_a_minute_of_ringing_still_follows_the_invite() {
    let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
    let inv = invite("longring-1@test", "tag-a", 7, "z9hG4bKr3");
    f.core.route_request(&inv, src()).await;

    tokio::time::advance(Duration::from_secs(60)).await;

    let cxl = cancel("longring-1@test", "tag-a", 7, "z9hG4bKr3c");
    let outcome = f.core.route_request(&cxl, src()).await;
    assert_eq!(outcome.decision, RoutingDecisionKind::Cancel);
    assert_eq!(outcome.target, Some(ProxyAddr::new(W1, 5060)), "CANCEL must follow the INVITE, not re-select");
    assert_eq!(f.strategy.calls.load(Ordering::SeqCst), 1, "no fallback selection for the CANCEL");
}

// Same window for the response side: a late non-2xx final must still find
// the INVITE entry, so the upstream's ACK for it still follows the
// INVITE's forward (relay, never a fresh selection).
#[tokio::test(start_paused = true)]
async fn late_non_2xx_final_ack_still_follows_the_invite() {
    let f = fixture(&[ProxyAddr::new(W1, 5060)]).await;
    let inv = invite("latefinal-1@test", "tag-a", 9, "z9hG4bKr4");
    f.core.route_request(&inv, src()).await;

    tokio::time::advance(Duration::from_secs(60)).await;

    let raw = format!(
        "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKr4\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: latefinal-1@test\r\n\
CSeq: 9 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    );
    let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
        panic!("expected response")
    };
    f.core.handle_response(resp).await;

    let out = f.core.route_request(&ack("latefinal-1@test", "tag-a", 9, "z9hG4bKr4"), src()).await;
    assert_eq!(out.decision, RoutingDecisionKind::AckHop);
    assert_eq!(out.target, Some(ProxyAddr::new(W1, 5060)), "the ACK must repeat the INVITE's forward");
    assert_eq!(f.strategy.calls.load(Ordering::SeqCst), 1, "no fresh selection for the ACK");
}

// Guards the From-tag in the entry key: without it the two directions of
// one Call-ID (independent CSeq spaces, both remembered here) overwrite
// each other when their CSeq numbers coincide, and a CANCEL is then
// forwarded to the wrong party with the wrong branch.
#[tokio::test(start_paused = true)]
async fn same_callid_same_cseq_different_from_tags_do_not_collide() {
    let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
    let dir_a = invite("glare-1@test", "tag-a", 5, "z9hG4bKa");
    let dir_b = invite("glare-1@test", "tag-b", 5, "z9hG4bKb");
    f.core.route_request(&dir_a, src()).await;
    f.core.route_request(&dir_b, src()).await;

    let cxl_a = cancel("glare-1@test", "tag-a", 5, "z9hG4bKac");
    let cxl_b = cancel("glare-1@test", "tag-b", 5, "z9hG4bKbc");
    let out_a = f.core.route_request(&cxl_a, src()).await;
    let out_b = f.core.route_request(&cxl_b, src()).await;

    assert_eq!(out_a.target, Some(ProxyAddr::new(W1, 5060)), "direction A's CANCEL follows A's INVITE");
    assert_eq!(out_b.target, Some(ProxyAddr::new(W2, 5060)), "direction B's CANCEL follows B's INVITE");
}
