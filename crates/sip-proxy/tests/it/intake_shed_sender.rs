//! The production intake shed at its watermark. On a single-face proxy a new
//! INVITE that reads as a worker's (its source or its top Via sent-by is a
//! registry worker, or its direction Route to the proxy carries `;outbound`)
//! belongs to a call that worker already accepted and is relayed; a new INVITE
//! with none of these signs is shed. The external face of a dual-face proxy
//! trusts none of them and sheds every new INVITE. An emergency INVITE is
//! relayed.
//!
//! The hook runs at watermark 0, so every datagram arrives at or above it and
//! no verdict depends on how far the core drained the queue. The core runs its
//! real recv loop on the simulated fabric.

use crate::common::call_end::{ack_final, answered_at_callee, busy_at_callee, request_of};
use crate::common::retransmits::{timer_a_copies, TIMER_B};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use sip_clock::Clock;
use sip_message::CustomParser;
use sip_net::types::PreIngressHook;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::intake_shed::intake_shed_hook;
use sip_proxy::registry::composed::ComposedWorkerRegistry;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{
    ExternalFaceParts, ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};
use sip_txn::IdGen;
use tokio::task::JoinHandle;
use topology::{Peer, SimulatedMembership};

/// The proxy's one face, or the internal face of the dual-face proxy.
const PROXY: &str = "192.0.2.1:5060";
/// The external face of the dual-face proxy.
const PROXY_EXT: &str = "192.0.2.2:5060";
/// The registry's one worker, and the target the strategy selects.
const WORKER: &str = "10.0.0.2:5070";
/// The node address a worker's datagrams carry when they are translated on
/// the way to the proxy.
const NODE: &str = "198.51.100.4:40001";
/// A worker on an address the registry does not hold.
const UNLISTED: &str = "10.0.0.9:5070";
/// A sender that is no worker.
const OUTSIDER: &str = "203.0.113.66:5060";
/// The Request-URI host of a worker's outbound INVITE.
const FAR: &str = "198.51.100.7:5060";
const QUEUE_MAX: usize = 64;

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

/// A registry that holds `WORKER` alone.
fn listing_the_worker() -> Arc<dyn WorkerRegistry> {
    Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
        "w1",
        ProxyAddr::from(addr(WORKER)),
    )]))
}

/// The production intake hook at watermark 0 for the face at `on` of a proxy
/// on `PROXY` routing over `registry`, its external face at `external` when
/// it has one.
fn hook_at_watermark_0(
    metrics: &Arc<ProxyMetrics>,
    _registry: &Arc<dyn WorkerRegistry>,
    _external: Option<&str>,
    _on: &str,
) -> PreIngressHook {
    intake_shed_hook(0, metrics.clone(), CustomParser::new())
}

struct Rig {
    task: JoinHandle<()>,
    metrics: Arc<ProxyMetrics>,
    worker: Box<dyn UdpEndpoint>,
    translated: Box<dyn UdpEndpoint>,
    unlisted: Box<dyn UdpEndpoint>,
    outsider: Box<dyn UdpEndpoint>,
    far: Box<dyn UdpEndpoint>,
}

impl Rig {
    /// A single-face proxy in front of `WORKER`.
    async fn single_face() -> Rig {
        Rig::at_the_watermark(listing_the_worker(), false).await
    }

    /// A proxy routing over `registry`, its sockets running the production
    /// intake hook at watermark 0: on `PROXY` alone, or with an external face
    /// on `PROXY_EXT` when `dual`.
    async fn at_the_watermark(registry: Arc<dyn WorkerRegistry>, dual: bool) -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), QUEUE_MAX));
        let metrics = Arc::new(ProxyMetrics::new());
        let external = dual.then_some(PROXY_EXT);
        let face = |at| {
            let hook = hook_at_watermark_0(&metrics, &registry, external, at);
            net.bind_udp(BindUdpOpts::new(addr(at), QUEUE_MAX).with_pre_ingress(hook))
        };
        let endpoint = face(PROXY).await.expect("bind the proxy");
        let worker = bind(WORKER).await.expect("bind the worker");
        let translated = bind(NODE).await.expect("bind the translated worker");
        let unlisted = bind(UNLISTED).await.expect("bind the unlisted worker");
        let outsider = bind(OUTSIDER).await.expect("bind the outsider");
        let far = bind(FAR).await.expect("bind the far end");
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::from(addr(WORKER))));
        let mut builder = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry.clone(),
            Arc::new(IdGen::seeded(0x5ed)),
            CustomParser::new(),
        )
        .clock(Clock::test_at(0))
        .metrics(metrics.clone());
        if dual {
            let endpoint = face(PROXY_EXT).await.expect("bind the external face");
            let advertised = ProxyAddr::from(addr(PROXY_EXT));
            builder = builder.external_face(ExternalFaceParts { endpoint, advertised });
        }
        let core = builder.build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, worker, translated, unlisted, outsider, far }
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    /// What `ep` read since its last drain.
    fn drained(ep: &dyn UdpEndpoint) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| ep.try_recv()).map(|p| p.raw.to_vec()).collect()
    }

    /// Nothing reached a worker or the far end.
    fn nothing_relayed(&self, what: &str) {
        for (party, ep) in [("worker", &self.worker), ("unlisted worker", &self.unlisted)] {
            assert!(Rig::drained(ep.as_ref()).is_empty(), "{what}: the {party} never sees it");
        }
        assert!(Rig::drained(self.far.as_ref()).is_empty(), "{what}: the far end never sees it");
    }

    /// The one datagram the far end read: the relayed INVITE.
    fn relayed_to_far(&self, what: &str) -> Vec<u8> {
        let at_far = Rig::drained(self.far.as_ref());
        let [forwarded] = at_far.as_slice() else {
            panic!("{what}: relayed to the far end once: {} read", at_far.len());
        };
        forwarded.clone()
    }

    /// `sender` sends `invite` to `entry` and resends it on every Timer A
    /// copy (RFC 3261 §17.1.1.2): each send is shed, counted on top of the
    /// `shed` already counted, and the transaction ends at Timer B with
    /// nothing heard. Returns the count after the last copy.
    async fn shed_through_timer_b(
        &self,
        sender: &dyn UdpEndpoint,
        entry: &str,
        invite: &[u8],
        what: &str,
        mut shed: u64,
    ) -> u64 {
        let sent_at = Instant::now();
        for at in std::iter::once(Duration::ZERO).chain(timer_a_copies()) {
            tokio::time::sleep_until(sent_at + at).await;
            sender.send_to(invite, addr(entry)).await.expect("sent");
            self.settle().await;
            shed += 1;
            self.nothing_relayed(what);
            assert_eq!(self.metrics.intake_shed(), shed, "{what}: the send at {at:?} is shed");
        }
        tokio::time::sleep_until(sent_at + TIMER_B).await;
        assert!(Rig::drained(sender).is_empty(), "{what}: nothing answers it");
        shed
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

/// The `;outbound` direction Route to the proxy face at `face`.
fn outbound_marker(face: &str) -> String {
    format!("Route: <sip:{face};lr;outbound>\r\n")
}

/// A registered worker places the outbound leg of a call it accepted: its new
/// INVITE is relayed to the Request-URI host at the watermark, none is shed,
/// and the far end's 486 and the worker's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_a_registered_workers_new_invite_is_relayed() {
    let rig = Rig::single_face().await;
    let invite = new_invite("worker-leg", WORKER, "");
    rig.worker.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forwarded = rig.relayed_to_far("the worker's new INVITE");
    assert_eq!(rig.metrics.intake_shed(), 0, "the worker's new INVITE is not shed");
    assert!(Rig::drained(rig.worker.as_ref()).is_empty(), "nothing comes back before the answer");
    busy_at_callee(addr(PROXY), rig.worker.as_ref(), &invite, rig.far.as_ref(), &forwarded).await;
}

/// A worker whose datagrams reach the proxy from a node address, its top Via
/// naming its registered sent-by: its new INVITE is relayed at the watermark.
/// The far end's 486 reaches the worker at its sent-by, and the ACK it sends
/// through the node address reaches the far end.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_a_translated_workers_new_invite_is_relayed() {
    let rig = Rig::single_face().await;
    let invite = new_invite("translated-leg", WORKER, "");
    rig.translated.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forwarded = rig.relayed_to_far("the translated worker's new INVITE");
    assert_eq!(rig.metrics.intake_shed(), 0, "the translated worker's new INVITE is not shed");
    let busy =
        answered_at_callee(rig.worker.as_ref(), rig.far.as_ref(), &forwarded, 486, "Busy Here")
            .await;
    ack_final(addr(PROXY), rig.translated.as_ref(), &invite, &busy).await;
    rig.settle().await;
    let [acked] = Rig::drained(rig.far.as_ref()).try_into().expect("one ACK at the far end");
    assert_eq!(request_of(&acked).method(), "ACK", "the worker's ACK reaches the far end");
}

/// A worker on an address the registry does not hold, its new INVITE routed
/// to the proxy by an `;outbound` direction Route: relayed at the watermark,
/// and the far end's 486 and the worker's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_an_unlisted_workers_new_invite_with_the_outbound_marker_is_relayed() {
    let rig = Rig::single_face().await;
    let invite = new_invite("marker-leg", UNLISTED, &outbound_marker(PROXY));
    rig.unlisted.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forwarded = rig.relayed_to_far("the marked new INVITE");
    assert_eq!(rig.metrics.intake_shed(), 0, "the marked new INVITE is not shed");
    busy_at_callee(addr(PROXY), rig.unlisted.as_ref(), &invite, rig.far.as_ref(), &forwarded).await;
}

/// A new INVITE with no sign of a worker, from a sender that is none: the
/// first send and every Timer A copy are shed, and the sender's transaction
/// ends at Timer B with nothing heard.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_an_outsiders_new_invite_is_shed() {
    let rig = Rig::single_face().await;
    let invite = new_invite("plain", OUTSIDER, "");
    rig.shed_through_timer_b(rig.outsider.as_ref(), PROXY, &invite, "plain", 0).await;
}

/// The external face of a dual-face proxy trusts no sign of a worker: a new
/// INVITE there is shed, every copy through Timer B, whether it carries an
/// `;outbound` Route marker to that face, a registry worker's Via sent-by, or
/// comes from a registry worker's address.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_the_external_face_sheds_a_new_invite_whatever_it_claims() {
    let rig = Rig::at_the_watermark(listing_the_worker(), true).await;
    let mut shed = 0;
    let marker = new_invite("ext-marker", OUTSIDER, &outbound_marker(PROXY_EXT));
    shed =
        rig.shed_through_timer_b(rig.outsider.as_ref(), PROXY_EXT, &marker, "marker", shed).await;
    let via = new_invite("ext-worker-via", WORKER, "");
    shed =
        rig.shed_through_timer_b(rig.outsider.as_ref(), PROXY_EXT, &via, "worker Via", shed).await;
    let source = new_invite("ext-worker-source", OUTSIDER, "");
    rig.shed_through_timer_b(rig.worker.as_ref(), PROXY_EXT, &source, "worker source", shed).await;
}

/// An outsider's emergency INVITE passes the shed and reaches the worker,
/// whose 486 and the outsider's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_an_outsiders_emergency_invite_is_relayed() {
    let rig = Rig::single_face().await;
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

/// A discovered pool lists a worker after the proxy bound its socket: the
/// worker's new INVITE is shed while the pool holds no worker at its address,
/// and its next Timer A copy, once the pool lists it, is relayed; the far
/// end's 486 and the worker's ACK of it complete the call.
#[tokio::test(start_paused = true)]
async fn at_the_watermark_a_worker_the_pool_lists_after_bind_is_relayed() {
    let membership = SimulatedMembership::with_clock(Vec::new(), Clock::test_at(0));
    let port = addr(WORKER).port();
    let registry =
        ComposedWorkerRegistry::spawn(Arc::new(membership.clone()), port, Clock::test_at(0));
    let rig = Rig::at_the_watermark(Arc::new(registry), false).await;
    let invite = new_invite("discovered-leg", WORKER, "");
    let sent_at = Instant::now();
    rig.worker.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    rig.nothing_relayed("before the listing");
    assert_eq!(rig.metrics.intake_shed(), 1, "shed while the pool does not list the worker");

    let host = addr(WORKER).ip().to_string();
    membership.add(Peer::new("w1", host));
    let first_copy = timer_a_copies()[0];
    tokio::time::sleep_until(sent_at + first_copy).await;
    rig.worker.send_to(&invite, addr(PROXY)).await.expect("sent");
    rig.settle().await;
    let forwarded = rig.relayed_to_far("the copy after the listing");
    assert_eq!(rig.metrics.intake_shed(), 1, "the copy after the listing is not shed");
    busy_at_callee(addr(PROXY), rig.worker.as_ref(), &invite, rig.far.as_ref(), &forwarded).await;
}
