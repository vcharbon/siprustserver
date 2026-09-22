//! What a core constructs is decided by its replication switch alone: an
//! unwired node (`B2buaDeps.replication = None`) exposes no replicating store,
//! no supervisor, no terminate writer and no fail-back sender; the router's
//! fail-back receiver exists only alongside that sender, created in the same
//! arm of the core's replication match. A wired node exposes all four.
//! Asserted by construction on the `Option`s, never by counting tasks.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork};
use sip_txn::IdGen;

use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::decision::ScriptedDecisionEngine;
use crate::limiter::NoopLimiter;
use crate::metrics::B2buaMetrics;
use crate::router::test_support;
use crate::store::InMemoryCallStore;
use crate::{B2buaCore, B2buaDeps};

/// A core on a simulated SIP fabric with replication not wired: the default
/// of both runners.
async fn unwired_core(ordinal: &str) -> B2buaCore {
    let sip_addr = SocketAddr::from(([127, 0, 0, 3], 5080));
    let endpoint = SimulatedSignalingNetwork::new(1)
        .bind_udp(BindUdpOpts::new(sip_addr, 256))
        .await
        .expect("bind the simulated SIP endpoint");
    let config = B2buaConfig {
        self_ordinal: ordinal.into(),
        sip_local_ip: sip_addr.ip().to_string(),
        sip_local_port: sip_addr.port(),
        ..Default::default()
    };
    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.3", 9)),
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(InMemoryCdrWriter::new()),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        clock: Clock::test_at(0),
        id_gen: Arc::new(IdGen::seeded(0xB2B2)),
        replication: None,
        metrics: B2buaMetrics::new(),
        adaptation_http: None,
        compose: crate::rules::ComposeOptions::default(),
    };
    B2buaCore::spawn(endpoint, deps)
}

/// With replication not wired, none of the replication parts exists.
#[tokio::test(start_paused = true)]
async fn an_unwired_core_constructs_no_replication_part() {
    let core = unwired_core("w0").await;
    assert!(core.repl_store().is_none(), "no replicating store on an unwired node");
    assert!(core.supervisor().is_none(), "no replication supervisor on an unwired node");
    assert!(core.terminate_writer().is_none(), "no terminate writer on an unwired node");
    assert!(
        core.fail_back_sender().is_none(),
        "no fail-back sender on an unwired node; the receiver exists only alongside it"
    );
}

/// The same accessors on a wired node say `Some` for every part, so the
/// unwired assertions above read the switch, not an accessor that never sees
/// anything.
#[tokio::test(start_paused = true)]
async fn a_wired_core_constructs_every_replication_part() {
    let node = test_support::node("w0").await;
    assert!(node.core.repl_store().is_some(), "a wired node holds its replicating store");
    assert!(node.core.supervisor().is_some(), "a wired node runs a supervisor");
    assert!(node.core.terminate_writer().is_some(), "a wired node drains through a writer");
    assert!(node.core.fail_back_sender().is_some(), "a wired node's router polls fail-back");
}
