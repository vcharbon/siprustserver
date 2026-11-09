//! The proxy's own OPTIONS senders bind the wildcard, so they name no address
//! of their own: what they state is no wildcard, and they ask `rport`, so the
//! answer finds the socket through a translation of its port (RFC 3581 §4).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_ids::SigningKey;
use sip_message::header::{HeaderName, Rport};
use sip_message::parser::custom::CustomParser;
use sip_message::received::stamp_received;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_net::{
    BindUdpOpts, SendError, SignalingNetwork, UdpEndpoint, UdpEndpointCounters, UdpPacket,
};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{DeadNotifier, HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

const SENDER: &str = "192.0.2.250:49152";
const W1: &str = "10.244.5.8:5060";
const W2: &str = "10.244.5.9:5060";
/// What a translation between the proxy and the workers makes of the source.
const TRANSLATED: &str = "192.0.2.250:33333";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

/// A socket bound to the wildcard: it reports `0.0.0.0` as its address.
struct Wildcard(Box<dyn UdpEndpoint>);

#[async_trait]
impl UdpEndpoint for Wildcard {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.0.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        self.0.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.0.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), self.0.local_addr().port())
    }
    fn queue_depth(&self) -> usize {
        self.0.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.0.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.0.counters()
    }
}

struct Rig {
    registry: SimulatedWorkerRegistry,
    clock: Clock,
    sender: Box<dyn UdpEndpoint>,
    workers: [Box<dyn UdpEndpoint>; 2],
}

async fn rig() -> Rig {
    let clock = Clock::test_at(1_000);
    let entry = |id: &str, at: &str| WorkerEntry::alive(id, ProxyAddr::from(addr(at)));
    let registry =
        SimulatedWorkerRegistry::with_clock(vec![entry("w1", W1), entry("w2", W2)], clock.clone());
    let net = SimulatedSignalingNetwork::new(1);
    let bind = |at: &str| net.bind_udp(BindUdpOpts::new(addr(at), 64));
    let sender: Box<dyn UdpEndpoint> =
        Box::new(Wildcard(bind(SENDER).await.expect("bind the sender")));
    let workers = [bind(W1).await.expect("bind w1"), bind(W2).await.expect("bind w2")];
    Rig { registry, clock, sender, workers }
}

async fn received(worker: &dyn UdpEndpoint) -> SipRequest {
    let packet = tokio::time::timeout(Duration::from_secs(5), worker.recv())
        .await
        .expect("an OPTIONS arrives")
        .expect("queue open");
    match CustomParser::new().parse(&packet.raw) {
        Ok(SipMessage::Request(req)) => req,
        other => panic!("a request: {other:?}"),
    }
}

/// What it states is no wildcard, and a worker answering by the RFC reaches the
/// translated source.
fn assert_reachable_through_a_translation(req: SipRequest) {
    let text = String::from_utf8_lossy(req.image()).to_string();
    assert!(!text.contains("0.0.0.0"), "a wildcard is no address: {text}");
    assert!(req.has(&HeaderName::Contact), "{text}");
    assert_eq!(req.top_via().host(), "proxy.invalid", "{text}");
    assert_eq!(req.top_via().rport(), Some(Rport::Requested), "{text}");
    assert_eq!(req.top_via().port(), Some(49152), "{text}");
    let answered = stamp_received(req, addr(TRANSLATED));
    assert_eq!(answered.destination, addr(TRANSLATED), "the answer finds the translated port");
}

#[tokio::test(start_paused = true)]
async fn the_probe_states_no_wildcard_and_asks_rport() {
    let r = rig().await;
    let registry = Arc::new(r.registry.clone());
    let control = r.registry.control();
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let probe = HealthProbe::new(
        r.sender,
        sip_message::CustomParser::new(),
        registry,
        control,
        observer,
        r.clock.clone(),
        Arc::new(IdGen::seeded(0x8460)),
        HealthProbeConfig::default(),
    );
    let task = tokio::spawn(probe.run());
    assert_reachable_through_a_translation(received(r.workers[0].as_ref()).await);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_dead_notice_states_no_wildcard_and_asks_rport() {
    let r = rig().await;
    let key = SigningKey::new(&[3u8; 32]).expect("a 32-byte key");
    let notifier = DeadNotifier::new(
        r.sender,
        sip_message::CustomParser::new(),
        Arc::new(r.registry.clone()),
        key,
        r.clock.clone(),
        Arc::new(IdGen::seeded(0x8461)),
    );
    let task = tokio::spawn(notifier.run());
    r.registry.set_health("w1", WorkerHealth::Dead);
    assert_reachable_through_a_translation(received(r.workers[1].as_ref()).await);
    task.abort();
}
