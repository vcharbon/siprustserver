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
use sip_net::{BindUdpOpts, PreIngressHook, SignalingNetwork};
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
    /// A proxy whose socket runs `hook`, its core holding `parser`, built over
    /// `metrics`.
    async fn new(
        metrics: Arc<ProxyMetrics>,
        hook: Option<PreIngressHook>,
        parser: CustomParser,
    ) -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), QUEUE_MAX));
        let opts = BindUdpOpts::new(addr(PROXY), QUEUE_MAX);
        let opts = match hook {
            Some(hook) => opts.with_pre_ingress(hook),
            None => opts,
        };
        let endpoint = net.bind_udp(opts).await.expect("bind the proxy");
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
        .parser(parser)
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
    let hook = intake_shed_hook(QUEUE_MAX, metrics.clone(), CustomParser::new());
    let rig = Rig::new(metrics, Some(hook), CustomParser::new()).await;

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

/// The limit a process states is the one its datagrams read under, whichever
/// stage parses them: the production hook below its watermark and the core with
/// no hook refuse the same well-formed OPTIONS past three header lines with no
/// partial, drop it unanswered, and count one parse error. The default limits
/// route the same datagram.
#[tokio::test(start_paused = true)]
async fn the_limit_the_process_states_is_read_at_every_stage() {
    let strict =
        || CustomParser::with_limits(SipParserLimits { max_header_lines: 3, ..Default::default() });
    let with_hook = {
        let metrics = Arc::new(ProxyMetrics::new());
        let hook = intake_shed_hook(QUEUE_MAX, metrics.clone(), strict());
        Rig::new(metrics, Some(hook), strict()).await
    };
    let no_hook = Rig::new(Arc::new(ProxyMetrics::new()), None, strict()).await;

    for rig in [&with_hook, &no_hook] {
        rig.caller.send_to(&options("strict"), addr(PROXY)).await.expect("sent");
        rig.settle().await;
        assert!(Rig::received(rig.worker.as_ref()).is_empty(), "nothing forwarded");
        assert!(Rig::received(rig.caller.as_ref()).is_empty(), "nothing answered");
        assert_eq!(rig.metrics.parse_errors(), 1, "the refusal is the one counted");
    }

    let default = Rig::new(Arc::new(ProxyMetrics::new()), None, CustomParser::new()).await;
    default.caller.send_to(&options("default"), addr(PROXY)).await.expect("sent");
    default.settle().await;
    assert_eq!(Rig::received(default.worker.as_ref()).len(), 1, "the default limits route it");
}

/// A request at every bound the proxy reads under, forwarded: its Via and
/// Record-Route lines and the `received`/`rport` on the top Via leave it past
/// those bounds, and an element behind the proxy reads what it forwards.
#[tokio::test(start_paused = true)]
async fn a_forward_at_every_bound_reads_behind_the_proxy() {
    let limits = SipParserLimits::default();
    let rig = Rig::new(Arc::new(ProxyMetrics::new()), None, CustomParser::new()).await;
    let head = "Via: ";
    let via = format!("SIP/2.0/UDP {CALLER};branch=z9hG4bK-bound;rport;x=");
    let via = format!("{via}{}", "a".repeat(limits.max_header_length - head.len() - via.len()));
    let mut lines = vec![
        "INVITE sip:bob@192.0.2.30 SIP/2.0".to_string(),
        format!("Via: {via}"),
        "Max-Forwards: 70".to_string(),
        "From: <sip:alice@192.0.2.20>;tag=alice-tag".to_string(),
        "To: <sip:bob@192.0.2.30>".to_string(),
        "Call-ID: bound@192.0.2.20".to_string(),
        "CSeq: 1 INVITE".to_string(),
        "Contact: <sip:alice@192.0.2.20:5060>".to_string(),
        "Content-Length: 0".to_string(),
    ];
    while lines.len() < limits.max_header_lines {
        lines.insert(2, "X-Pad: p".to_string());
    }
    let invite = format!("{}\r\n\r\n", lines.join("\r\n"));
    assert!(CustomParser::new().parse(invite.as_bytes()).is_ok(), "the request is at its bounds");

    rig.caller.send_to(invite.as_bytes(), addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forward = rig.worker.try_recv().expect("the proxy forwards it");

    let default = CustomParser::new().parse(&forward.raw).expect_err("past the default bounds");
    assert!(default.reason.contains("exceeds limit"), "{}", default.reason);
    let behind = CustomParser::with_limits(SipParserLimits::default().behind_proxy());
    assert!(behind.parse(&forward.raw).is_ok(), "an element behind the proxy reads the forward");
}
