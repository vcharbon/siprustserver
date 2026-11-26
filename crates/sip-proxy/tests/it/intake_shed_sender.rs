//! The production intake shed on a single-face proxy, at its watermark: a new
//! INVITE a registered worker sends belongs to a call that worker already
//! accepted and is relayed; a new INVITE from any other sender is shed, whatever
//! its Via or Route claims; an emergency INVITE is relayed.
//!
//! The hook runs at watermark 0, so every datagram arrives at or above it and
//! no verdict depends on how far the core drained the queue. The core runs its
//! real recv loop on the simulated fabric.

use crate::common::call_end::busy_at_callee;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::CustomParser;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::intake_shed::intake_shed_hook;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

const PROXY: &str = "192.0.2.1:5060";
/// The registry's one worker, and the target the strategy selects.
const WORKER: &str = "10.0.0.2:5070";
/// A sender that is no worker.
const OUTSIDER: &str = "203.0.113.66:5060";
/// The Request-URI host of a worker's outbound INVITE.
const FAR: &str = "198.51.100.7:5060";
const QUEUE_MAX: usize = 64;

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

struct Rig {
    task: JoinHandle<()>,
    metrics: Arc<ProxyMetrics>,
    worker: Box<dyn UdpEndpoint>,
    outsider: Box<dyn UdpEndpoint>,
    far: Box<dyn UdpEndpoint>,
}

impl Rig {
    /// A single-face proxy in front of `WORKER`, its socket running the
    /// production intake hook at watermark 0.
    async fn at_the_watermark() -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), QUEUE_MAX));
        let metrics = Arc::new(ProxyMetrics::new());
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
                "w1",
                ProxyAddr::from(addr(WORKER)),
            )]));
        let hook = intake_shed_hook(0, metrics.clone(), CustomParser::new());
        let opts = BindUdpOpts::new(addr(PROXY), QUEUE_MAX).with_pre_ingress(hook);
        let endpoint = net.bind_udp(opts).await.expect("bind the proxy");
        let worker = bind(WORKER).await.expect("bind the worker");
        let outsider = bind(OUTSIDER).await.expect("bind the outsider");
        let far = bind(FAR).await.expect("bind the far end");
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry,
            Arc::new(IdGen::seeded(0x1194)),
            CustomParser::new(),
        )
        .clock(Clock::test_at(0))
        .metrics(metrics.clone())
        .build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, worker, outsider, far }
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    /// What `ep` read since its last drain.
    fn drained(ep: &dyn UdpEndpoint) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| ep.try_recv()).map(|p| p.raw.to_vec()).collect()
    }

    /// Nothing reached the worker or the far end.
    fn nothing_relayed(&self, what: &str) {
        assert!(Rig::drained(self.worker.as_ref()).is_empty(), "{what}: no worker sees it");
        assert!(Rig::drained(self.far.as_ref()).is_empty(), "{what}: the far end never sees it");
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A new INVITE toward `FAR` whose top Via names `sent_by`, with `extra`
/// header lines.
fn new_invite(call_id: &str, sent_by: &str, extra: &str) -> Vec<u8> {
    let host = sent_by.split(':').next().expect("a host");
    format!(
        "INVITE sip:+15550100@{FAR} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {sent_by};branch=z9hG4bK-{call_id};rport\r\n\
         {extra}Max-Forwards: 70\r\n\
         From: <sip:caller@{host}>;tag=c1\r\n\
         To: <sip:+15550100@{FAR}>\r\n\
         Call-ID: {call_id}@{host}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:caller@{sent_by}>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A registered worker places the outbound leg of a call it accepted: its new
/// INVITE is relayed to the Request-URI host at the watermark, none is shed,
/// and the far end's 486 and the worker's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_a_registered_workers_new_invite_is_relayed() {
    let rig = Rig::at_the_watermark().await;
    let invite = new_invite("worker-leg", WORKER, "");
    rig.worker.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let at_far = Rig::drained(rig.far.as_ref());
    let [forwarded] = at_far.as_slice() else {
        panic!("the worker's new INVITE is relayed to the far end once: {} read", at_far.len());
    };
    assert_eq!(rig.metrics.intake_shed(), 0, "the worker's new INVITE is not shed");
    assert!(Rig::drained(rig.worker.as_ref()).is_empty(), "nothing comes back before the answer");
    busy_at_callee(addr(PROXY), rig.worker.as_ref(), &invite, rig.far.as_ref(), forwarded).await;
}

/// The source address is the evidence: an outsider's new INVITE is shed, and
/// so is one claiming the worker's Via sent-by or carrying an `;outbound`
/// Route marker to the proxy.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_an_outsiders_new_invite_is_shed_whatever_it_claims() {
    let rig = Rig::at_the_watermark().await;
    let marker = format!("Route: <sip:{PROXY};lr;outbound>\r\n");
    for (call_id, sent_by, extra) in [
        ("plain", OUTSIDER, String::new()),
        ("worker-via", WORKER, String::new()),
        ("marker", OUTSIDER, marker),
    ] {
        let invite = new_invite(call_id, sent_by, &extra);
        rig.outsider.send_to(&invite, addr(PROXY)).await.expect("sent");
        rig.settle().await;
        rig.nothing_relayed(call_id);
        assert!(Rig::drained(rig.outsider.as_ref()).is_empty(), "{call_id}: nothing answers it");
    }
    assert_eq!(rig.metrics.intake_shed(), 3, "each is counted shed");
}

/// An outsider's emergency INVITE passes the shed and reaches the worker,
/// whose 486 and the outsider's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_an_outsiders_emergency_invite_is_relayed() {
    let rig = Rig::at_the_watermark().await;
    let invite = new_invite("emergency", OUTSIDER, "Resource-Priority: esnet.0\r\n");
    rig.outsider.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let at_worker = Rig::drained(rig.worker.as_ref());
    let [forwarded] = at_worker.as_slice() else {
        panic!("the emergency INVITE reaches the worker once: {} read", at_worker.len());
    };
    assert_eq!(rig.metrics.intake_shed(), 0, "an emergency INVITE is not shed");
    busy_at_callee(addr(PROXY), rig.outsider.as_ref(), &invite, rig.worker.as_ref(), forwarded)
        .await;
}
