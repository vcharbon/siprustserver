//! Recv-loop sharding end-to-end (Pass 9): TWO `ProxyCore` instances on two
//! SO_REUSEPORT sockets sharing one port (and the same strategy/registry/
//! cancel-LRU/metrics Arcs), real UDP. Many UAC source sockets fire OPTIONS at
//! the shared port; every one must be forwarded to the worker and its 200
//! routed back — including when the worker's reply flow-hashes to a DIFFERENT
//! shard than the one that forwarded the request (the response path is
//! stateless, any shard can serve it; the shared LRU covers CANCEL/rtx memos).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{serialize, SipMessage, SipParser};
use sip_net::types::BindUdpOpts;
use sip_net::{RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use sip_proxy::cancel_lru::CancelBranchLru;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::time::timeout;

/// Endpoint decorator counting datagrams its shard actually received — the
/// bound endpoint's own counters are only snapshot-readable and the endpoint
/// moves into the core.
struct CountingEndpoint {
    inner: Box<dyn UdpEndpoint>,
    received: Arc<AtomicU64>,
}

#[async_trait]
impl UdpEndpoint for CountingEndpoint {
    async fn send_to(&self, buf: &[u8], dst: std::net::SocketAddr) -> Result<(), sip_net::SendError> {
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<sip_net::UdpPacket> {
        let pkt = self.inner.recv().await;
        if pkt.is_some() {
            self.received.fetch_add(1, Ordering::Relaxed);
        }
        pkt
    }
    fn try_recv(&self) -> Option<sip_net::UdpPacket> {
        let pkt = self.inner.try_recv();
        if pkt.is_some() {
            self.received.fetch_add(1, Ordering::Relaxed);
        }
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

#[tokio::test]
async fn two_shards_serve_many_flows_and_cross_shard_responses() {
    let net = RealSignalingNetwork::new();

    // Worker: answers every OPTIONS with 200 (from ITS one socket — so all its
    // replies form ONE flow toward the proxy port and land on ONE shard,
    // whichever it is, exercising the cross-shard response path).
    let worker = net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 256)).await.unwrap();
    let worker_addr = worker.local_addr();
    let _responder = tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = worker.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let opts = GenerateResponseOpts { to_tag: Some("uas".into()), ..Default::default() };
            let resp = generate_response(&req, 200, "OK", &opts);
            let _ = worker.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    });

    // Two reuse-port shard endpoints on one ephemeral port.
    let ep0 = net
        .bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 256).with_reuse_port(true))
        .await
        .unwrap();
    let proxy_addr = ep0.local_addr();
    let ep1 = net.bind_udp(BindUdpOpts::new(proxy_addr, 256).with_reuse_port(true)).await.unwrap();
    let (r0, r1) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let ep0: Box<dyn UdpEndpoint> = Box::new(CountingEndpoint { inner: ep0, received: r0.clone() });
    let ep1: Box<dyn UdpEndpoint> = Box::new(CountingEndpoint { inner: ep1, received: r1.clone() });

    // Shared routing state across the shards (the production wiring shape).
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::from(worker_addr)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let clock = Clock::system();
    let metrics = Arc::new(ProxyMetrics::new());
    let lru = Arc::new(CancelBranchLru::with_clock(clock.clone()));
    let mut tasks = Vec::new();
    for (shard, ep) in [ep0, ep1].into_iter().enumerate() {
        let core = ProxyCoreBuilder::new(ProxyAddr::from(proxy_addr), strategy.clone(), registry.clone())
            .clock(clock.clone())
            .id_gen(Arc::new(IdGen::seeded(0xBEEF + shard as u64)))
            .metrics(metrics.clone())
            .cancel_lru(lru.clone())
            .shard(shard)
            .build(ep);
        tasks.push(tokio::spawn(core.run()));
    }

    // 16 UAC flows (distinct src ports) — enough that both shards get traffic
    // with overwhelming probability under the kernel 4-tuple hash.
    const FLOWS: usize = 16;
    let mut uacs = Vec::new();
    for i in 0..FLOWS {
        let uac = net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64)).await.unwrap();
        uac.send_to(&options(uac.local_addr().port(), i, proxy_addr), proxy_addr).await.unwrap();
        uacs.push(uac);
    }

    // Every UAC gets its 200 back, whatever shard served either direction.
    let parser = CustomParser::new();
    for (i, uac) in uacs.iter().enumerate() {
        let pkt = timeout(Duration::from_secs(3), uac.recv())
            .await
            .unwrap_or_else(|_| panic!("uac-{i} never got its 200 back"))
            .unwrap();
        match parser.parse(&pkt.raw) {
            Ok(SipMessage::Response(resp)) => {
                assert_eq!(resp.status, 200, "uac-{i}");
                assert_eq!(resp.call_id, format!("shard-e2e-{i}@127.0.0.1"));
            }
            other => panic!("uac-{i} expected a 200 response, got {other:?}"),
        }
    }

    // The load actually sharded: each core counted what its own socket
    // received, and together they saw all inbound traffic (16 requests + 16
    // worker replies). Both shards took part.
    let (n0, n1) = (r0.load(Ordering::Relaxed), r1.load(Ordering::Relaxed));
    assert_eq!(n0 + n1, FLOWS as u64 * 2, "16 UAC requests + 16 worker replies, no drops");
    assert!(n0 > 0 && n1 > 0, "both shards must have received traffic (got {n0}/{n1})");

    for t in tasks {
        t.abort();
    }
}
