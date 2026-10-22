//! A request the parser refuses is answered `400 Invalid Request` by the
//! proxy's stateless path when its Via / From / To / Call-ID / CSeq read
//! (RFC 3261 §16.3): to the packet's source, every copy answered again, the
//! ACK to the 400 absorbed (the proxy is that INVITE's UAS) and nothing
//! forwarded. A response, an ACK and a request no response can echo are
//! dropped unanswered; the 400s are capped by a token bucket, an answer it
//! refuses dropped and counted.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::{CustomParser, HeaderName, SipMessage, SipParser, SipResponse};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::parse_refusal::PARSE_REFUSAL_BURST;
use sip_txn::IdGen;
use tokio::task::JoinHandle;

const PROXY: &str = "192.0.2.1:5060";
const CALLER: &str = "192.0.2.20:5060";
const WORKER: &str = "10.0.0.2:5070";
const FROM: &str = "<sip:alice@192.0.2.20>;tag=alice-tag";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

struct Rig {
    task: JoinHandle<()>,
    metrics: Arc<ProxyMetrics>,
    caller: Box<dyn UdpEndpoint>,
    worker: Box<dyn UdpEndpoint>,
}

impl Rig {
    async fn new() -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 1024));
        let endpoint = bind(PROXY).await.expect("bind the proxy");
        let caller = bind(CALLER).await.expect("bind the caller");
        let worker = bind(WORKER).await.expect("bind the worker");
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let metrics = Arc::new(ProxyMetrics::new());
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry,
            Arc::new(IdGen::seeded(0x400)),
        )
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, caller, worker }
    }

    async fn send(&self, datagram: &[u8]) {
        self.caller.send_to(datagram, addr(PROXY)).await.expect("sent");
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    fn to_caller(&self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.caller.try_recv()).map(|p| p.raw).collect()
    }

    fn to_worker(&self) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.worker.try_recv()).map(|p| p.raw).collect()
    }

    fn counters(&self) -> (u64, u64, u64) {
        let m = &self.metrics;
        (m.parse_errors(), m.parse_refusals_answered(), m.parse_refusals_capped())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An INVITE whose Max-Forwards breaks the grammar; its other lines read.
fn malformed_invite(branch: &str, from: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@192.0.2.30 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {CALLER};branch={branch}\r\n\
         Max-Forwards: abc\r\n\
         From: {from}\r\n\
         To: <sip:bob@192.0.2.30>\r\n\
         Call-ID: refused@192.0.2.20\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@{CALLER}>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn ack(branch: &str, to_tag: &str, max_forwards: &str) -> Vec<u8> {
    format!(
        "ACK sip:bob@192.0.2.30 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {CALLER};branch={branch}\r\n\
         Max-Forwards: {max_forwards}\r\n\
         From: {FROM}\r\n\
         To: <sip:bob@192.0.2.30>;tag={to_tag}\r\n\
         Call-ID: refused@192.0.2.20\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn response(raw: &[u8]) -> SipResponse {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Response(r)) => r,
        other => panic!("a response: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_malformed_invite_is_answered_400_each_copy_and_its_ack_is_absorbed() {
    let rig = Rig::new().await;
    let invite = malformed_invite("z9hG4bK-bad-1", FROM);

    let mut tags = Vec::new();
    for copy in 1..=2 {
        rig.send(&invite).await;
        rig.settle().await;
        let sent = rig.to_caller();
        assert_eq!(sent.len(), 1, "copy {copy} draws one answer");
        let resp = response(&sent[0]);
        assert_eq!((resp.status(), resp.reason()), (400, "Invalid Request"));
        assert_eq!(resp.top_via().branch(), Some("z9hG4bK-bad-1"));
        let warning: Vec<_> = resp.raw_text(HeaderName::Warning).collect();
        assert_eq!(warning.len(), 1, "one Warning");
        assert!(
            warning[0].as_str().starts_with("399 192.0.2.1:5060 \"Invalid Max-Forwards"),
            "{}",
            warning[0].as_str()
        );
        tags.push(resp.to().tag().expect("a To-tag").to_string());
        assert!(rig.to_worker().is_empty(), "copy {copy} is not forwarded");
    }
    assert_eq!(tags[0], tags[1], "every copy draws the same To-tag (§8.2.6.2)");

    rig.send(&ack("z9hG4bK-bad-1", &tags[0], "70")).await;
    rig.settle().await;
    assert!(rig.to_caller().is_empty(), "the ACK draws nothing");
    assert!(rig.to_worker().is_empty(), "the ACK to the proxy's own 400 ends here");
    assert_eq!(rig.counters(), (0, 2, 0));
}

#[tokio::test(start_paused = true)]
async fn a_malformed_ack_is_dropped_unanswered() {
    let rig = Rig::new().await;
    rig.send(&ack("z9hG4bK-bad-ack", "bob-tag", "abc")).await;
    rig.settle().await;
    assert!(rig.to_caller().is_empty(), "an ACK is never answered");
    assert!(rig.to_worker().is_empty());
    assert_eq!(rig.counters(), (1, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_malformed_response_is_dropped_unanswered() {
    let rig = Rig::new().await;
    let resp = format!(
        "SIP/2.0 486 Busy Here\r\n\
         Via: SIP/2.0/UDP {PROXY};branch=z9hG4bK-out\r\n\
         Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-up\r\n\
         From: {FROM}\r\n\
         To: <sip:bob@192.0.2.30>;tag=bob-tag\r\n\
         Call-ID: resp@192.0.2.20\r\n\
         CSeq: 1 INVITE\r\n\
         Expires: x\r\n\
         Content-Length: 0\r\n\r\n"
    );
    rig.send(resp.as_bytes()).await;
    rig.settle().await;
    assert!(rig.to_caller().is_empty(), "a response is never answered");
    assert_eq!(rig.counters(), (1, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_request_with_a_malformed_from_is_dropped_unanswered() {
    let rig = Rig::new().await;
    rig.send(&malformed_invite("z9hG4bK-bad-from", "<sip:alice@192.0.2.20>junk;tag=a")).await;
    rig.settle().await;
    assert!(rig.to_caller().is_empty(), "no From to echo: no answer");
    assert!(rig.to_worker().is_empty());
    assert_eq!(rig.counters(), (1, 0, 0));
}

#[tokio::test(start_paused = true)]
async fn past_the_burst_a_refusal_is_dropped_and_counted() {
    let rig = Rig::new().await;
    let burst = usize::try_from(PARSE_REFUSAL_BURST).expect("a count");
    for i in 0..=burst {
        rig.send(&malformed_invite(&format!("z9hG4bK-flood-{i}"), FROM)).await;
    }
    rig.settle().await;
    assert_eq!(rig.to_caller().len(), burst, "the burst is answered, the one past it is not");
    assert_eq!(rig.counters(), (0, u64::from(PARSE_REFUSAL_BURST), 1));
}

/// The ordinary counterpart: a well-formed INVITE is forwarded, draws no
/// 400 and moves no refusal counter.
#[tokio::test(start_paused = true)]
async fn well_formed_traffic_draws_no_400() {
    let rig = Rig::new().await;
    let invite = String::from_utf8(malformed_invite("z9hG4bK-good", FROM))
        .expect("text")
        .replace("Max-Forwards: abc", "Max-Forwards: 70");
    rig.send(invite.as_bytes()).await;
    rig.settle().await;
    assert_eq!(rig.to_worker().len(), 1, "forwarded");
    assert!(
        rig.to_caller().iter().all(|raw| response(raw).status() != 400),
        "no 400 to a well-formed request"
    );
    assert_eq!(rig.counters(), (0, 0, 0));
}
