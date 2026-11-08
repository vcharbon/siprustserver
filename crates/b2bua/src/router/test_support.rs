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
use crate::decision::{CallDecisionEngine, ScriptedDecisionEngine};
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
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.2", 9));
    node_deciding(net, ordinal, decision, tune, services).await
}

/// [`node_on`] consulting `decision`.
pub async fn node_deciding(
    net: &SimulatedSignalingNetwork,
    ordinal: &str,
    decision: Arc<dyn CallDecisionEngine>,
    tune: impl FnOnce(&mut B2buaConfig),
    services: Vec<crate::rules::ServiceDef>,
) -> Node {
    node_probed(net, ordinal, decision, tune, services, Default::default()).await
}

/// [`node_deciding`] reporting its turns to `turn_probe`.
pub async fn node_probed(
    net: &SimulatedSignalingNetwork,
    ordinal: &str,
    decision: Arc<dyn CallDecisionEngine>,
    tune: impl FnOnce(&mut B2buaConfig),
    services: Vec<crate::rules::ServiceDef>,
    turn_probe: crate::turn_probe::TurnProbes,
) -> Node {
    let clock = Clock::test_at(0);
    let sip_addr = SocketAddr::from(NODE_SIP_ADDR);
    let endpoint = net
        .bind_udp(BindUdpOpts::new(sip_addr, 256))
        .await
        .expect("bind the simulated SIP endpoint");
    let store = Arc::new(ReplicatingCallStore::new(1, clock.clone()));
    // The node's own endpoint is published, so it reads itself routable.
    let own = Peer::new(ordinal, ordinal);
    let view = Arc::new(SimulatedMembership::with_clock(vec![own], clock.clone()));
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
        verdict: Default::default(),
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
        decision,
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(cdr.clone()),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        turn_probe,
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

/// An answered call `pri` owns and `bak` backs up, as its INVITE's turn and
/// the caller's ACK left it: the caller's dialog confirmed under the
/// INVITE's tags, nothing in flight.
pub fn answered(pri: &str, bak: &str, cid: &str) -> call::Call {
    let config = B2buaConfig { self_ordinal: pri.into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    let mut call = crate::initial_invite::build_initial_call(
        &invite(pri, bak, cid),
        src(),
        &config,
        &mut ids,
        0,
    );
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call.a_leg.own_tag = Some("b2b-tag".into());
    call::testkit::stage_leg(&mut call.a_leg, call::LegState::Confirmed);
    let dialog = call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    dialog.peer_tag = "alicetag".into();
    dialog.peer_cseq = Some(1);
    dialog.remote_target = "sip:alice@10.0.0.9:5060".into();
    call
}

/// The caller's BYE of `call`'s dialog ([`answered`]), naming no `callRef`
/// of ours.
pub fn caller_bye(call: &call::Call) -> Vec<u8> {
    format!(
        "BYE sip:b2bua@127.0.0.2:5080 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-bye.{}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>;tag=b2b-tag\r\n\
         Call-ID: {}\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n",
        call.a_leg.call_id.replace('@', "."),
        call.a_leg.call_id
    )
    .into_bytes()
}

/// Put `call`, placed as its INVITE placed it, into the `role` partition of
/// `primary` in `store`, indexed under its keys.
pub async fn put_body(
    store: &ReplicatingCallStore,
    role: crate::store::PartitionRole,
    primary: &str,
    call: &call::Call,
) {
    use crate::store::CallStore;
    let body = crate::store::envelope::placed_body(call).expect("a placed body");
    let keys = call::call_index_keys(call);
    let opts = crate::store::PutOpts::default();
    let put = store.put_call(role, primary, &call.call_ref, body, &keys, 600_000, 1, 0, &opts);
    put.await.expect("the body is stored");
}

/// The status lines `peer` receives within `within`.
pub async fn heard(peer: &dyn sip_net::UdpEndpoint, within: std::time::Duration) -> Vec<String> {
    let mut lines = Vec::new();
    let deadline = tokio::time::Instant::now() + within;
    while let Ok(Some(p)) = tokio::time::timeout_at(deadline, peer.recv()).await {
        let text = String::from_utf8_lossy(&p.raw).to_string();
        lines.push(text.lines().next().unwrap_or_default().to_string());
    }
    lines
}
