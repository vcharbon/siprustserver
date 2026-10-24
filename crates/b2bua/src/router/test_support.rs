//! One wired core for the router's own unit tests: a real router, store, timer
//! driver and transaction layer on simulated SIP + replication fabrics, with no
//! peers — the store is seeded by hand and nothing pulls it. Tests that drive a
//! router seam directly ([`crate::router::materialise`], [`crate::router::reclaim`])
//! share this scaffolding so the seam under test is the only thing each file
//! spells out.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::IdGen;
use topology::{Membership, Peer, SimulatedMembership};

use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::decision::ScriptedDecisionEngine;
use crate::limiter::NoopLimiter;
use crate::metrics::B2buaMetrics;
use crate::repl::{AddrResolver, FnPeerResolver, PullerConfig, ReplicatingCallStore, ServeConfig};
use crate::store::InMemoryCallStore;
use crate::{B2buaCore, B2buaDeps, ReplicationSetup};

/// A running core for `ordinal` with its replicating store, CDR sink, counters
/// and clock in hand.
pub struct Node {
    pub core: B2buaCore,
    /// The node's membership view: a peer added then removed departs.
    pub membership: Arc<SimulatedMembership>,
    pub store: Arc<ReplicatingCallStore>,
    pub cdr: InMemoryCdrWriter,
    pub metrics: B2buaMetrics,
    pub clock: Clock,
    /// The model-invariant record's mark at the node's start: the node fails
    /// its test on drop with the first violation kept after it.
    since: usize,
}

impl Node {
    /// The violations kept since the node's start or the last take, which the
    /// node then no longer fails its test with: a test that builds a defect
    /// takes the ones it expects.
    pub fn take_violations(&mut self) -> Vec<String> {
        let kept = crate::model_invariants::violations();
        let taken = kept.get(self.since..).unwrap_or_default().to_vec();
        self.since = kept.len();
        taken
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Some(first) = crate::model_invariants::violations().get(self.since) {
            panic!("{first}");
        }
    }
}

/// Spawn a core on simulated SIP and replication fabrics with no peers.
pub async fn node(ordinal: &str) -> Node {
    node_with(ordinal, |_| {}).await
}

/// [`node`] with `tune` applied to its config last.
pub async fn node_with(ordinal: &str, tune: impl FnOnce(&mut B2buaConfig)) -> Node {
    node_serving(ordinal, tune, Vec::new()).await
}

/// [`node_with`] running `services`.
pub async fn node_serving(
    ordinal: &str,
    tune: impl FnOnce(&mut B2buaConfig),
    services: Vec<crate::rules::ServiceDef>,
) -> Node {
    node_on(&SimulatedSignalingNetwork::new(1), ordinal, tune, services).await
}

/// The SIP address a test node listens on.
pub const NODE_SIP_ADDR: ([u8; 4], u16) = ([127, 0, 0, 2], 5080);

/// [`node_serving`] on the SIP fabric `net`, where a test may bind peers.
pub async fn node_on(
    net: &SimulatedSignalingNetwork,
    ordinal: &str,
    tune: impl FnOnce(&mut B2buaConfig),
    services: Vec<crate::rules::ServiceDef>,
) -> Node {
    let clock = Clock::test_at(0);
    let sip_addr = SocketAddr::from(NODE_SIP_ADDR);
    let endpoint = net
        .bind_udp(BindUdpOpts::new(sip_addr, 256))
        .await
        .expect("bind the simulated SIP endpoint");
    let store = Arc::new(ReplicatingCallStore::new(1, clock.clone()));
    let view = Arc::new(SimulatedMembership::with_clock(Vec::new(), clock.clone()));
    let membership: Arc<dyn Membership> = view.clone();
    let addr_resolver: AddrResolver =
        Arc::new(FnPeerResolver(|_: &Peer| SocketAddr::from(([127, 0, 0, 2], 1))));
    let network = SimulatedReplicationNetwork::with_delay(1);
    let listener = network.listen(SocketAddr::from(([127, 0, 0, 2], 9600))).await.unwrap();
    let setup = ReplicationSetup {
        network: Arc::new(network),
        membership,
        store: store.clone(),
        listener,
        addr_resolver,
        incarnation_gen: 1,
        puller: PullerConfig::default(),
        serve: ServeConfig::default(),
        replica: crate::node::replica::ReplicaConfig::default(),
        write_path: Default::default(),
    };
    let config = B2buaConfig {
        self_ordinal: ordinal.into(),
        sip_local_ip: sip_addr.ip().to_string(),
        sip_local_port: sip_addr.port(),
        keepalive_interval_sec: 300,
        ..Default::default()
    };
    let mut config = config;
    tune(&mut config);
    let cdr = InMemoryCdrWriter::new();
    let metrics = B2buaMetrics::new();
    let deps = B2buaDeps {
        config,
        identity: String::new(),
        decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.2", 9)),
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(cdr.clone()),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        turn_probe: Default::default(),
        clock: clock.clone(),
        id_gen: Arc::new(IdGen::seeded(0xB2B1)),
        refusals: None,
        deferred_ceilings: None,
        replication: Some(setup),
        metrics: metrics.clone(),
        adaptation_http: None,
        compose: crate::rules::ComposeOptions::default(),
        capacity: None,
    };
    Node {
        core: B2buaCore::spawn_with_services(endpoint, deps, services),
        membership: view,
        store,
        cdr,
        metrics,
        clock,
        since: crate::model_invariants::mark(),
    }
}

pub use crate::test_call::{invite, src, turn};
