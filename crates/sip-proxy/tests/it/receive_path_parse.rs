//! The proxy core takes the parse its receive path made, as on a production
//! signaling socket: the intake hook parses each datagram before it is queued
//! and the core routes the queued message, or answers the queued refusal with
//! its stateless 400 (RFC 3261 §16.3), without parsing the datagram again.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, SipMessage, SipParser, SipParserLimits};
use sip_net::UdpEndpoint;
use sip_net::{Arrival, BindUdpOpts, PreIngressAction, PreIngressHook, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::intake_shed::intake_shed_hook;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

const PROXY: &str = "192.0.2.1:5060";
const CALLER: &str = "192.0.2.20:5060";
const WORKER: &str = "10.0.0.2:5070";
const QUEUE_MAX: usize = 1024;

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
    /// A proxy whose socket runs `hook`, built over `metrics`.
    async fn new(metrics: Arc<ProxyMetrics>, hook: PreIngressHook) -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), QUEUE_MAX));
        let endpoint = net
            .bind_udp(BindUdpOpts::new(addr(PROXY), QUEUE_MAX).with_pre_ingress(hook))
            .await
            .expect("bind the proxy");
        let caller = bind(CALLER).await.expect("bind the caller");
        let worker = bind(WORKER).await.expect("bind the worker");
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry,
            Arc::new(IdGen::seeded(0x645)),
        )
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, caller, worker }
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    fn received(ep: &dyn UdpEndpoint) -> Vec<SipMessage> {
        std::iter::from_fn(|| ep.try_recv())
            .map(|p| CustomParser::new().parse(&p.raw).expect("the proxy sends what parses"))
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn options(call_id: &str) -> Vec<u8> {
    format!(
        "OPTIONS sip:bob@192.0.2.30 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-{call_id}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@192.0.2.20>;tag=alice-tag\r\n\
         To: <sip:bob@192.0.2.30>\r\n\
         Call-ID: {call_id}@192.0.2.20\r\n\
         CSeq: 1 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An INVITE whose Max-Forwards breaks the grammar; the lines a 400 echoes
/// read.
const MALFORMED_INVITE: &[u8] = b"INVITE sip:bob@192.0.2.30 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.20:5060;branch=z9hG4bK-malformed\r\n\
Max-Forwards: abc\r\n\
From: <sip:alice@192.0.2.20>;tag=alice-tag\r\n\
To: <sip:bob@192.0.2.30>\r\n\
Call-ID: malformed@192.0.2.20\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@192.0.2.20:5060>\r\n\
Content-Length: 0\r\n\r\n";

/// Under the production intake hook, below its watermark: a well-formed
/// OPTIONS is routed to the worker and its 200 relayed back, and a malformed
/// INVITE is answered 400 from the refusal queued with it, nothing forwarded.
#[tokio::test(start_paused = true)]
async fn the_core_routes_and_answers_the_parse_the_production_hook_queued() {
    let metrics = Arc::new(ProxyMetrics::new());
    let rig = Rig::new(metrics.clone(), intake_shed_hook(QUEUE_MAX, metrics)).await;

    rig.caller.send_to(&options("routed"), addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forwarded = Rig::received(rig.worker.as_ref());
    let [SipMessage::Request(options)] = forwarded.as_slice() else {
        panic!("the OPTIONS reaches the worker: {forwarded:?}");
    };
    let ok = generate_response(
        options,
        200,
        "OK",
        &GenerateResponseOpts { to_tag: Some("bob-tag".into()), ..GenerateResponseOpts::default() },
    )
    .expect("a 200");
    let wire = sip_message::serialize(&SipMessage::Response(ok));
    rig.worker.send_to(&wire, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let relayed = Rig::received(rig.caller.as_ref());
    assert!(
        matches!(relayed.as_slice(), [SipMessage::Response(r)] if r.status() == 200),
        "the 200 is relayed to the caller: {relayed:?}"
    );

    rig.caller.send_to(MALFORMED_INVITE, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let answered = Rig::received(rig.caller.as_ref());
    assert!(
        matches!(answered.as_slice(), [SipMessage::Response(r)] if r.status() == 400),
        "the queued refusal is answered 400: {answered:?}"
    );
    assert!(Rig::received(rig.worker.as_ref()).is_empty(), "nothing forwarded");
    assert_eq!(rig.metrics.parse_refusals_answered(), 1);
}

/// The core acts on the queued parse, not on one of its own: a hook whose
/// parser refuses every datagram past three header lines queues a
/// well-formed OPTIONS with that refusal (no partial), and the core drops it
/// unanswered where its own parse would have routed it.
#[tokio::test(start_paused = true)]
async fn the_core_acts_on_the_queued_parse_not_its_own() {
    let strict =
        CustomParser::with_limits(SipParserLimits { max_header_lines: 3, ..Default::default() });
    let hook: PreIngressHook = Arc::new(move |arrival: &mut Arrival, _src, _depth| {
        arrival.parse(&strict);
        PreIngressAction::Accept
    });
    let rig = Rig::new(Arc::new(ProxyMetrics::new()), hook).await;

    rig.caller.send_to(&options("strict"), addr(PROXY)).await.expect("sent");
    rig.settle().await;

    assert!(Rig::received(rig.worker.as_ref()).is_empty(), "nothing forwarded");
    assert!(Rig::received(rig.caller.as_ref()).is_empty(), "nothing answered");
    assert_eq!(rig.metrics.parse_errors(), 1, "the queued refusal is the one counted");
}
