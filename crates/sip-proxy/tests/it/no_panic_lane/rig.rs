//! One case of the proxy lane: a proxy core over the simulated fabric on its
//! own paused-clock runtime, with the load balancer the runner wires (HMAC
//! cookies, two workers), a caller, the two workers and a callee as peers,
//! and a resolver that names hosts the callee's address, so no name a
//! datagram carries leaves the process. The case replays its seed's
//! unmutated preamble, sends the mutated datagram twice and the seed's
//! follow-up, and runs past the proxy's sweeps.
//!
//! A [`Variant`] reaches what the peer drives beside the plain rig: the
//! proxy's own refusals (its self-gate closed, no worker alive), an owner
//! that is down, the dual-face edge, names that resolve to nothing, and the
//! health probe reading the workers' OPTIONS answers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::resolver::HostResolver;
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::self_gate::{AdmitDecision, ProxySelfGate, ShedReason};
use sip_proxy::{
    ExternalFaceParts, FaceCidrs, LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
};
use sip_txn::IdGen;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Where the proxy listens: its one face, or its external face where it has
/// two.
pub const PROXY: (&str, u16) = ("192.0.2.1", 5060);
/// The proxy's internal face, where it has two: the workers' plane.
pub const PROXY_INTERNAL: (&str, u16) = ("10.0.0.1", 5060);
/// The internal plane of a dual-face proxy.
const INTERNAL_CIDRS: &str = "10.0.0.0/24";
/// Where the health probe listens.
pub const PROBE: (&str, u16) = ("10.0.0.1", 5061);
/// The peers, by role.
pub const CALLER: (&str, u16) = ("192.0.2.20", 5060);
pub const WORKER_A: (&str, u16) = ("10.0.0.2", 5070);
pub const WORKER_B: (&str, u16) = ("10.0.0.3", 5070);
pub const CALLEE: (&str, u16) = ("192.0.2.30", 5060);

/// The gap between two datagrams of a case: a UDP peer's first T1.
pub const GAP: Duration = Duration::from_millis(500);

/// How long a case runs after its last datagram: past the proxy's memo
/// sweeps and its resolver's lookup timeout.
const RUN_PAST_SWEEPS: Duration = Duration::from_secs(40);

pub fn addr((host, port): (&str, u16)) -> SocketAddr {
    SocketAddr::new(host.parse().expect("a lane address"), port)
}

/// The peer a datagram of the lane leaves from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    Caller,
    WorkerA,
    WorkerB,
    Callee,
}

/// How a case failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The recv loop ended: a panic on the loop every call shares.
    LoopEnded,
    /// The parse guard caught a parser panic.
    ParserPanicked,
    /// The health probe's loop ended: a panic reading a worker's answer.
    ProbeEnded,
}

/// What the rig runs beside the plain proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Two alive workers, one face, every name resolved.
    Plain,
    /// The self-gate refuses every external new-dialog INVITE.
    GateClosed,
    /// No worker is alive: a request with no target is refused.
    NoWorker,
    /// This worker is down: its dialogs fall back to the other owner.
    WorkerDown(Peer),
    /// Two faces: callers on the external one, workers on the internal one.
    DualFace,
    /// Some names resolve to nothing: the resolver's negative entries.
    ResolverGaps,
    /// The health probe runs, the workers' answers feeding the observer.
    Probed,
}

impl Variant {
    /// Every variant a datagram case runs on.
    pub const DATAGRAM: [Variant; 7] = [
        Variant::Plain,
        Variant::GateClosed,
        Variant::NoWorker,
        Variant::WorkerDown(Peer::WorkerA),
        Variant::WorkerDown(Peer::WorkerB),
        Variant::DualFace,
        Variant::ResolverGaps,
    ];

    /// The variant its seeds are recorded on: one that forwards them.
    pub fn recorded_on(self) -> Variant {
        match self {
            Variant::DualFace => Variant::DualFace,
            _ => Variant::Plain,
        }
    }

    fn health(self, worker: Peer) -> WorkerHealth {
        match self {
            Variant::NoWorker => WorkerHealth::Dead,
            Variant::WorkerDown(down) if down == worker => WorkerHealth::Dead,
            _ => WorkerHealth::Alive,
        }
    }
}

/// A name resolves to the callee's host, on the port asked; with `gaps`, a
/// name of odd length resolves to nothing.
struct CalleeResolver {
    gaps: bool,
}

#[async_trait]
impl HostResolver for CalleeResolver {
    async fn resolve(&self, host: &str, port: u16, _deadline: Instant) -> Vec<SocketAddr> {
        if self.gaps && host.len() % 2 == 1 {
            return Vec::new();
        }
        vec![SocketAddr::new(addr(CALLEE).ip(), port)]
    }
}

/// A self-gate that refuses every external new-dialog INVITE.
struct ClosedGate;

impl ProxySelfGate for ClosedGate {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: 5 }
    }
}

/// A running proxy and its peers.
pub struct Rig {
    pub task: JoinHandle<()>,
    pub metrics: Arc<ProxyMetrics>,
    pub observer: Arc<WorkerLoadObserver>,
    /// The health probe's loop, in the probed variant.
    pub probe: Option<JoinHandle<()>>,
    variant: Variant,
    peers: [Box<dyn UdpEndpoint>; 4],
}

fn worker(id: &str, at: (&str, u16), health: WorkerHealth) -> WorkerEntry {
    WorkerEntry { health, ..WorkerEntry::alive(id, ProxyAddr::new(at.0, at.1)) }
}

impl Rig {
    pub async fn with(variant: Variant) -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 4096));
        let dual = variant == Variant::DualFace;
        let face = if dual { PROXY_INTERNAL } else { PROXY };
        let endpoint = bind(face).await.expect("bind the proxy");
        let peers = [
            bind(CALLER).await.expect("bind the caller"),
            bind(WORKER_A).await.expect("bind worker a"),
            bind(WORKER_B).await.expect("bind worker b"),
            bind(CALLEE).await.expect("bind the callee"),
        ];
        let clock = Clock::test_at(0);
        let workers = Arc::new(StaticWorkerRegistry::from_entries_with_clock(
            vec![
                worker("b2b-a", WORKER_A, variant.health(Peer::WorkerA)),
                worker("b2b-b", WORKER_B, variant.health(Peer::WorkerB)),
            ],
            clock.clone(),
        ));
        let registry: Arc<dyn WorkerRegistry> = workers.clone();
        let hmac = Arc::new(
            StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).expect("a key"),
        );
        let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
        let metrics = Arc::new(ProxyMetrics::new());
        let strategy = Arc::new(LoadBalancerStrategy::new(
            registry.clone(),
            hmac,
            observer.clone(),
            metrics.clone(),
            clock.clone(),
        ));
        let id_gen = Arc::new(IdGen::seeded(0x1a5e));
        let advertised = ProxyAddr::new(face.0, face.1);
        let gaps = variant == Variant::ResolverGaps;
        let mut builder = ProxyCoreBuilder::new(advertised, strategy, registry.clone(), id_gen)
            .clock(clock.clone())
            .metrics(metrics.clone())
            .resolver(Arc::new(CalleeResolver { gaps }));
        if dual {
            builder = builder.external_face(ExternalFaceParts {
                endpoint: bind(PROXY).await.expect("bind the external face"),
                advertised: ProxyAddr::new(PROXY.0, PROXY.1),
                int_cidrs: FaceCidrs::parse(INTERNAL_CIDRS).expect("the internal plane"),
            });
        }
        if variant == Variant::GateClosed {
            builder = builder.self_gate(Arc::new(ClosedGate));
        }
        let core = builder.build(endpoint);
        let probe = match variant {
            Variant::Probed => {
                let endpoint = bind(PROBE).await.expect("bind the probe");
                let probe = HealthProbe::new(
                    endpoint,
                    registry,
                    workers.control(),
                    observer.clone(),
                    clock,
                    Arc::new(IdGen::seeded(0x9be)),
                    HealthProbeConfig::default(),
                );
                Some(tokio::spawn(probe.run()))
            }
            _ => None,
        };
        Rig { task: tokio::spawn(core.run()), metrics, observer, probe, variant, peers }
    }

    /// Where `from` reaches the proxy: a worker its internal face where it has
    /// two.
    fn face_for(&self, from: Peer) -> SocketAddr {
        match (self.variant, from) {
            (Variant::DualFace, Peer::WorkerA | Peer::WorkerB) => addr(PROXY_INTERNAL),
            _ => addr(PROXY),
        }
    }

    fn peer(&self, peer: Peer) -> &dyn UdpEndpoint {
        let [caller, a, b, callee] = &self.peers;
        match peer {
            Peer::Caller => caller.as_ref(),
            Peer::WorkerA => a.as_ref(),
            Peer::WorkerB => b.as_ref(),
            Peer::Callee => callee.as_ref(),
        }
    }

    /// Send `datagram` from `from` to the proxy, then wait a [`GAP`].
    pub async fn send(&self, from: Peer, datagram: &[u8]) {
        let _ = self.peer(from).send_to(datagram, self.face_for(from)).await;
        tokio::time::sleep(GAP).await;
    }

    /// Send `datagram` from `from` to the health probe, then wait a [`GAP`].
    pub async fn send_to_probe(&self, from: Peer, datagram: &[u8]) {
        let _ = self.peer(from).send_to(datagram, addr(PROBE)).await;
        tokio::time::sleep(GAP).await;
    }

    /// Run the clock past the proxy's sweeps and its resolver's timeout.
    pub async fn run_past_sweeps(&self) {
        tokio::time::sleep(RUN_PAST_SWEEPS).await;
    }

    /// Every datagram `peer` has received, in order.
    pub fn drain(&self, peer: Peer) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.peer(peer).try_recv()).map(|p| p.raw.to_vec()).collect()
    }

    /// The failure the case ended in, if any.
    pub fn failure(&self) -> Option<Failure> {
        if self.task.is_finished() {
            Some(Failure::LoopEnded)
        } else if self.probe.as_ref().is_some_and(JoinHandle::is_finished) {
            Some(Failure::ProbeEnded)
        } else if self.metrics.parse_panics() > 0 {
            Some(Failure::ParserPanicked)
        } else {
            None
        }
    }
}

/// What a case did: its failure, and how much of the proxy it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub failure: Option<Failure>,
    /// Datagrams the proxy sent any peer.
    pub forwarded: usize,
}

/// One case on the plain rig: `before`, the mutated `datagram` from `from`
/// twice, `after`.
pub fn case(
    before: &[(Peer, Vec<u8>)],
    from: Peer,
    datagram: &[u8],
    after: &[(Peer, Vec<u8>)],
) -> Outcome {
    case_on(Variant::Plain, before, from, datagram, after)
}

/// The lane's runtime: one thread, its clock paused.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("the lane runtime")
}

/// [`case`] on the rig of `variant`.
pub fn case_on(
    variant: Variant,
    before: &[(Peer, Vec<u8>)],
    from: Peer,
    datagram: &[u8],
    after: &[(Peer, Vec<u8>)],
) -> Outcome {
    runtime().block_on(async {
        let rig = Rig::with(variant).await;
        for (peer, bytes) in before {
            rig.send(*peer, bytes).await;
        }
        let peers = [Peer::Caller, Peer::WorkerA, Peer::WorkerB, Peer::Callee];
        for peer in peers {
            rig.drain(peer);
        }
        rig.send(from, datagram).await;
        rig.send(from, datagram).await;
        let forwarded = peers.iter().map(|p| rig.drain(*p).len()).sum::<usize>();
        for (peer, bytes) in after {
            rig.send(*peer, bytes).await;
        }
        rig.run_past_sweeps().await;
        Outcome { failure: rig.failure(), forwarded }
    })
}
