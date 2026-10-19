//! One case of the proxy lane: a proxy core over the simulated fabric on its
//! own paused-clock runtime, with the load balancer the runner wires (HMAC
//! cookies, two alive workers), a caller, the two workers and a callee as
//! peers. The case replays its seed's unmutated preamble, sends the mutated
//! datagram twice and the seed's follow-up, and runs past the proxy's sweeps.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

/// Where the proxy listens.
pub const PROXY: (&str, u16) = ("192.0.2.1", 5060);
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
}

/// A running proxy and its peers.
pub struct Rig {
    pub task: JoinHandle<()>,
    pub metrics: Arc<ProxyMetrics>,
    peers: [Box<dyn UdpEndpoint>; 4],
}

impl Rig {
    pub async fn new() -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let bind = |at| net.bind_udp(BindUdpOpts::new(addr(at), 4096));
        let endpoint = bind(PROXY).await.expect("bind the proxy");
        let peers = [
            bind(CALLER).await.expect("bind the caller"),
            bind(WORKER_A).await.expect("bind worker a"),
            bind(WORKER_B).await.expect("bind worker b"),
            bind(CALLEE).await.expect("bind the callee"),
        ];
        let clock = Clock::test_at(0);
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries_with_clock(
                vec![
                    WorkerEntry::alive("b2b-a", ProxyAddr::new(WORKER_A.0, WORKER_A.1)),
                    WorkerEntry::alive("b2b-b", ProxyAddr::new(WORKER_B.0, WORKER_B.1)),
                ],
                clock.clone(),
            ));
        let hmac = Arc::new(
            StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).expect("a key"),
        );
        let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
        let metrics = Arc::new(ProxyMetrics::new());
        let strategy = Arc::new(LoadBalancerStrategy::new(
            registry.clone(),
            hmac,
            observer,
            metrics.clone(),
            clock.clone(),
        ));
        let id_gen = Arc::new(IdGen::seeded(0x1a5e));
        let advertised = ProxyAddr::new(PROXY.0, PROXY.1);
        let core = ProxyCoreBuilder::new(advertised, strategy, registry, id_gen)
            .clock(clock)
            .metrics(metrics.clone())
            .build(endpoint);
        Rig { task: tokio::spawn(core.run()), metrics, peers }
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
        let _ = self.peer(from).send_to(datagram, addr(PROXY)).await;
        tokio::time::sleep(GAP).await;
    }

    /// Every datagram `peer` has received, in order.
    pub fn drain(&self, peer: Peer) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| self.peer(peer).try_recv()).map(|p| p.raw).collect()
    }

    /// The failure the case ended in, if any.
    pub fn failure(&self) -> Option<Failure> {
        if self.task.is_finished() {
            Some(Failure::LoopEnded)
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

/// One case: `before`, the mutated `datagram` from `from` twice, `after`.
pub fn case(
    before: &[(Peer, Vec<u8>)],
    from: Peer,
    datagram: &[u8],
    after: &[(Peer, Vec<u8>)],
) -> Outcome {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("the lane runtime");
    runtime.block_on(async {
        let rig = Rig::new().await;
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
        tokio::time::sleep(RUN_PAST_SWEEPS).await;
        Outcome { failure: rig.failure(), forwarded }
    })
}
