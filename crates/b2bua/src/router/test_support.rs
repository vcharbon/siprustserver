//! One wired core for the router's own unit tests: a real router, store, timer
//! driver and transaction layer on simulated SIP + replication fabrics, with no
//! peers — the store is seeded by hand and nothing pulls it. Tests that drive a
//! router seam directly ([`crate::router::materialise`], [`crate::router::reclaim`])
//! share this scaffolding so the seam under test is the only thing each file
//! spells out.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork};
use sip_txn::IdGen;
use topology::{Membership, Peer, SimulatedMembership};

use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::decision::ScriptedDecisionEngine;
use crate::limiter::NoopLimiter;
use crate::metrics::B2buaMetrics;
use crate::repl::{AddrResolver, FnPeerResolver, ReplicatingCallStore};
use crate::store::InMemoryCallStore;
use crate::{B2buaCore, B2buaDeps, ReplicationSetup};

/// A running core for `ordinal` with its replicating store, CDR sink, counters
/// and clock in hand.
pub struct Node {
    pub core: B2buaCore,
    pub store: Arc<ReplicatingCallStore>,
    pub cdr: InMemoryCdrWriter,
    pub metrics: B2buaMetrics,
    pub clock: Clock,
}

/// Spawn a core on simulated SIP and replication fabrics with no peers.
pub async fn node(ordinal: &str) -> Node {
    let clock = Clock::test_at(0);
    let sip_addr = SocketAddr::from(([127, 0, 0, 2], 5080));
    let endpoint = SimulatedSignalingNetwork::new(1)
        .bind_udp(BindUdpOpts::new(sip_addr, 256))
        .await
        .expect("bind the simulated SIP endpoint");
    let store = Arc::new(ReplicatingCallStore::new(1, clock.clone()));
    let membership: Arc<dyn Membership> =
        Arc::new(SimulatedMembership::with_clock(Vec::new(), clock.clone()));
    let addr_resolver: AddrResolver =
        Arc::new(FnPeerResolver(|_: &Peer| SocketAddr::from(([127, 0, 0, 2], 1))));
    let setup = ReplicationSetup {
        network: Arc::new(SimulatedReplicationNetwork::with_delay(1)),
        membership,
        store: store.clone(),
        listen_addr: SocketAddr::from(([127, 0, 0, 2], 9600)),
        addr_resolver,
        incarnation_gen: 1,
    };
    let config = B2buaConfig {
        self_ordinal: ordinal.into(),
        sip_local_ip: sip_addr.ip().to_string(),
        sip_local_port: sip_addr.port(),
        keepalive_interval_sec: 300,
        reboot_budget_sec: 600,
        ..Default::default()
    };
    let cdr = InMemoryCdrWriter::new();
    let metrics = B2buaMetrics::new();
    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.2", 9)),
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(cdr.clone()),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        clock: clock.clone(),
        id_gen: Arc::new(IdGen::seeded(0xB2B1)),
        replication: Some(setup),
        metrics: metrics.clone(),
        adaptation_http: None,
        compose: crate::rules::ComposeOptions::default(),
    };
    Node { core: B2buaCore::spawn(endpoint, deps), store, cdr, metrics, clock }
}

/// The caller's source address on the simulated fabric.
pub fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

/// A proxied INVITE carrying the `w_pri`/`w_bak` cookie, keyed by Call-ID so
/// each call gets a distinct `callRef` (`{pri}|{cid}@…|alicetag`).
pub fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-{cid}\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {cid}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}
