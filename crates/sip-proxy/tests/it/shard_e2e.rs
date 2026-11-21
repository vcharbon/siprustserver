//! Recv-loop sharding end to end: two `ProxyCore` instances on two
//! SO_REUSEPORT sockets sharing one port (and the same strategy, registry,
//! cancel memo and metrics), over real UDP. Many UAC source sockets send
//! OPTIONS to the shared port; each is forwarded to the worker and its 200
//! routed back, including when the worker's reply flow hashes to a shard other
//! than the one that forwarded the request: the response path is stateless,
//! and the shared memo covers CANCEL and retransmission entries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use sip_proxy::cancel_lru::CancelBranchLru;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::time::timeout;

/// What one shard received: every datagram, and the requests among them.
#[derive(Default)]
struct ShardCounts {
    received: AtomicU64,
    requests: AtomicU64,
}

impl ShardCounts {
    fn count(&self, pkt: &Option<sip_net::UdpPacket>) {
        let Some(pkt) = pkt else { return };
        self.received.fetch_add(1, Ordering::Relaxed);
        if !pkt.raw.starts_with(b"SIP/2.0 ") {
            self.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// Endpoint decorator counting datagrams its shard actually received — the
/// bound endpoint's own counters are only snapshot-readable and the endpoint
/// moves into the core.
struct CountingEndpoint {
    inner: Box<dyn UdpEndpoint>,
    counts: Arc<ShardCounts>,
}

#[async_trait]
impl UdpEndpoint for CountingEndpoint {
    async fn send_to(
        &self,
        buf: &[u8],
        dst: std::net::SocketAddr,
    ) -> Result<(), sip_net::SendError> {
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<sip_net::UdpPacket> {
        let pkt = self.inner.recv().await;
        self.counts.count(&pkt);
        pkt
    }
    fn try_recv(&self) -> Option<sip_net::UdpPacket> {
        let pkt = self.inner.try_recv();
        self.counts.count(&pkt);
        pkt
    }
    fn local_addr(&self) -> std::net::SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> sip_net::UdpEndpointCounters {
        self.inner.counters()
    }
}

fn options(uac_port: u16, i: usize, dst: std::net::SocketAddr) -> Vec<u8> {
    format!(
        "OPTIONS sip:keepalive@{dst} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{uac_port};branch=z9hG4bK-shard-{i}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:uac-{i}@127.0.0.1>;tag=t-{i}\r\n\
         To: <sip:keepalive@127.0.0.1>\r\n\
         Call-ID: shard-e2e-{i}@127.0.0.1\r\n\
         CSeq: 1 OPTIONS\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The UAC flows a run opens at most while it waits for the kernel's
/// reuseport hash to place a request on each shard: the chance that 64 flows
/// all hash to one shard is 2⁻⁶³.
const MAX_FLOWS: usize = 64;

/// The UAC flows that send at once once each shard has received a request.
const FLOWS: usize = 16;

/// UAC `i` receives the 200 to its OPTIONS.
async fn answered(uac: &dyn UdpEndpoint, i: usize) {
    let pkt = timeout(Duration::from_secs(3), uac.recv())
        .await
        .unwrap_or_else(|_| panic!("uac-{i} never got its 200 back"))
        .unwrap();
    match CustomParser::new().parse(&pkt.raw) {
        Ok(SipMessage::Response(resp)) => {
            assert_eq!(resp.status(), 200, "uac-{i}");
            assert_eq!(resp.call_id().as_str(), format!("shard-e2e-{i}@127.0.0.1"));
        }
        other => panic!("uac-{i} expected a 200 response, got {other:?}"),
    }
}

/// The worker answers from its one socket, so all its replies form one flow
/// and land on one shard, whichever shard forwarded each request. UAC flows
/// open, each awaiting its 200, until each shard has received a request: one
/// of them forwarded a request whose 200 came back through the other. Then
/// many flows send at once, and every one is answered.
#[tokio::test]
async fn two_shards_serve_many_flows_and_cross_shard_responses() {
    let net = RealSignalingNetwork::new();

    let worker = net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 256)).await.unwrap();
    let worker_addr = worker.local_addr();
    let _responder = tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = worker.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let opts = GenerateResponseOpts { to_tag: Some("uas".into()), ..Default::default() };
            let resp = generate_response(&req, 200, "OK", &opts).unwrap();
            let _ = worker.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    });

    let ep0 = net
        .bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 256).with_reuse_port(true))
        .await
        .unwrap();
    let proxy_addr = ep0.local_addr();
    let ep1 = net.bind_udp(BindUdpOpts::new(proxy_addr, 256).with_reuse_port(true)).await.unwrap();
    let (c0, c1) = (Arc::new(ShardCounts::default()), Arc::new(ShardCounts::default()));
    let ep0: Box<dyn UdpEndpoint> = Box::new(CountingEndpoint { inner: ep0, counts: c0.clone() });
    let ep1: Box<dyn UdpEndpoint> = Box::new(CountingEndpoint { inner: ep1, counts: c1.clone() });

    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::from(worker_addr)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let clock = Clock::system();
    let metrics = Arc::new(ProxyMetrics::new());
    let lru = Arc::new(CancelBranchLru::with_clock(clock.clone()));
    let mut tasks = Vec::new();
    for (shard, ep) in [ep0, ep1].into_iter().enumerate() {
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(proxy_addr),
            strategy.clone(),
            registry.clone(),
            Arc::new(IdGen::seeded(0xBEEF + shard as u64)),
            sip_message::CustomParser::new(),
        )
        .clock(clock.clone())
        .metrics(metrics.clone())
        .cancel_lru(lru.clone())
        .shard(shard)
        .build(ep);
        tasks.push(tokio::spawn(core.run()));
    }

    let mut uacs = Vec::new();
    while uacs.len() < MAX_FLOWS && (c0.requests() == 0 || c1.requests() == 0) {
        let i = uacs.len();
        let uac = net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64)).await.unwrap();
        uac.send_to(&options(uac.local_addr().port(), i, proxy_addr), proxy_addr).await.unwrap();
        answered(&*uac, i).await;
        uacs.push(uac);
    }
    let (q0, q1) = (c0.requests(), c1.requests());
    assert!(q0 > 0 && q1 > 0, "after {MAX_FLOWS} flows a shard received no request ({q0}/{q1})");

    let probes = uacs.len();
    for i in probes..probes + FLOWS {
        let uac = net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64)).await.unwrap();
        uac.send_to(&options(uac.local_addr().port(), i, proxy_addr), proxy_addr).await.unwrap();
        uacs.push(uac);
    }
    for (i, uac) in uacs.iter().enumerate().skip(probes) {
        answered(&**uac, i).await;
    }

    let flows = uacs.len() as u64;
    assert_eq!(c0.requests() + c1.requests(), flows, "each flow's request reached one shard");
    assert_eq!(c0.received() + c1.received(), flows * 2, "each request and its 200, no drops");

    for t in tasks {
        t.abort();
    }
}
