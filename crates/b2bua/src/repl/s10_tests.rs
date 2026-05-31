//! S10a tests: wiring the (S4–S8) replication engine into the LIVE b2bua call
//! path — the `CallState`-flush-replicates seam, the Record-Route cookie →
//! `CallTopology` parse, and the centralized callGen bump-point.
//!
//! These do NOT stand up the full SIP failover harness (that is S10b). They
//! exercise S10a's three resolved seams directly:
//!
//! 1. a `CallState` wired `with_replication` flushes a backed-up call through the
//!    S8 write-side policy so the body lands on the peer's `ReplicatingCallStore`
//!    (proving the b2bua flush → changelog → ReplServer → puller path end-to-end);
//! 2. the proxy's `w_pri`/`w_bak` stickiness cookie (URI params on the topmost
//!    Record-Route) reaches `topology.pri`/`topology.bak` via `build_initial_call`;
//! 3. each authoritative `CallState::update` increments `topology.gen` (the LWW
//!    content version) from the gen=1 baseline stamped at INVITE time.
//!
//! All paused-clock tests drive the protocol BETWEEN `advance`s and use transit
//! delay `>= 1 ms` (CLAUDE.md fake-clock hazards).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use call::{Call, MsgpackCodec, CallBodyCodec};
use repl_net::transport::{ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use topology::{Peer, SimulatedMembership};

use super::{
    Changelog, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor,
};
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::metrics::B2buaMetrics;
use crate::store::{BufferedTerminateWriter, CallState, CallStore, PartitionRole};

const BAK: PartitionRole = PartitionRole::Backup;

fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 100,
        backoff_max_ms: 1_000,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Drive the paused clock in 100 ms chunks (CLAUDE.md: advance between protocol
/// steps; settle yields so spawned tasks make progress at each step).
async fn tick(ms: u64) {
    let chunks = ms.div_ceil(100).max(1);
    for _ in 0..chunks {
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
}

/// A replicating node: its store + changelog + a ReplServer running in the bg.
struct Node {
    store: Arc<ReplicatingCallStore>,
    addr: SocketAddr,
}

impl Node {
    async fn spawn(
        ordinal: &str,
        addr: SocketAddr,
        gen: u64,
        net: &Arc<SimulatedReplicationNetwork>,
        clock: &Clock,
    ) -> Self {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(30_000, 300_000);
        let store = Arc::new(ReplicatingCallStore::with_changelog(
            changelog.clone(),
            clock.clone(),
        ));
        let listener = net.listen(addr).await.unwrap();
        let server = ReplServer::new(ordinal, changelog, store.clone());
        tokio::spawn(server.run(listener));
        Self { store, addr }
    }
}

fn supervisor_for(
    self_ordinal: &str,
    store: &ReplicatingCallStore,
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
    addrs: Vec<(String, SocketAddr)>,
) -> ReplicationSupervisor {
    let map: std::collections::HashMap<String, SocketAddr> = addrs.into_iter().collect();
    let resolve = Arc::new(move |peer: &Peer| *map.get(&peer.ordinal).unwrap());
    ReplicationSupervisor::with_config(
        self_ordinal,
        net.clone(),
        store.clone(),
        resolve,
        clock.clone(),
        fast_config(),
    )
}

fn one_peer(ordinal: &str, clock: &Clock) -> Arc<SimulatedMembership> {
    Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new(ordinal, ordinal)],
        clock.clone(),
    ))
}

/// Build a `B2buaConfig` for a worker `ordinal` (only `self_ordinal` matters here).
fn config_for(ordinal: &str) -> B2buaConfig {
    B2buaConfig {
        self_ordinal: ordinal.into(),
        ..Default::default()
    }
}

/// Craft + parse a raw INVITE carrying the proxy's stickiness cookie as URI
/// params on the topmost Record-Route. `pri`/`bak` become `w_pri`/`w_bak`.
fn invite_with_cookie(pri: &str, bak: &str) -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-s10\r\n\
         Record-Route: <sip:10.0.0.1:5060;v=3;w_pri={pri};w_bak={bak};e=0;kid=k1;sig=abc;lr>\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: call-s10a@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    }
}

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

// ---------------------------------------------------------------------------
// (1) A replicating b2bua's CallState flush lands the call body on the peer.
//
// Two replicating nodes w0/w1 over a shared sim repl network. w1 pulls w0 (so it
// backs w0 up). A CallState wired to w0's store + a terminate-writer draining to
// that same store flushes a call whose topology = {pri:w0, bak:w1, gen:1}. The
// flush rides the S8 write-side policy → changelog-for-w1 (partition=Bak) →
// ReplServer streams it → w1's puller imports it as bak:w0. We assert w1 holds
// the body, and that it decodes back to the same call.
// ---------------------------------------------------------------------------
#[tokio::test(start_paused = true)]
async fn replicating_callstate_flush_lands_on_peer() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));

    let w0 = Node::spawn("w0", SocketAddr::from(([127, 0, 0, 1], 9901)), 1, &net, &clock).await;
    let w1 = Node::spawn("w1", SocketAddr::from(([127, 0, 0, 1], 9902)), 1, &net, &clock).await;

    // w1 backs up w0: w1's supervisor pulls w0.
    let w1_sup = supervisor_for("w1", &w1.store, &net, &clock, vec![("w0".into(), w0.addr)]);
    w1_sup.start(one_peer("w0", &clock));
    tick(50).await;

    // The b2bua call path: a CallState over w0's store, draining to that store so
    // the changelog bumps on the flush (exactly how B2buaCore wires it).
    let writer = BufferedTerminateWriter::spawn(
        (w0.store.clone()) as Arc<dyn CallStore>,
        1024,
    );
    let state = CallState::new(w0.store.clone() as Arc<dyn CallStore>, writer, "w0", B2buaMetrics::new())
        .with_replication(w0.store.clone());

    // Build a call from an INVITE carrying the w_pri=w0;w_bak=w1 cookie. The
    // callRef encodes primary w0, so the write-side policy routes it Forward → w1.
    let invite = invite_with_cookie("w0", "w1");
    let call = build_initial_call(&invite, src(), &config_for("w0"), 0);
    assert_eq!(
        call.topology.as_ref().map(|t| (t.pri.as_str(), t.bak.as_str(), t.gen)),
        Some(("w0", "w1", 1)),
        "cookie → topology stamped on the call"
    );
    let call_ref = call.call_ref.clone();
    assert!(call_ref.starts_with("w0|"), "callRef encodes primary w0");

    state.create(call.clone());
    state.flush(&call);
    // Let the writer drain → changelog bump → ReplServer push → w1 import.
    tick(150).await;

    let got = w1.store.get_call(BAK, "w0", &call_ref).await.unwrap();
    assert!(got.is_some(), "w1 holds w0's call in bak:w0");
    let decoded = MsgpackCodec::new().decode(got.as_deref().unwrap()).unwrap();
    assert_eq!(decoded.call_ref, call_ref, "replicated body round-trips to the same call");
    assert_eq!(
        w1.store.current_call_gen(BAK, "w0", &call_ref),
        Some(1),
        "replicated at the gen=1 baseline"
    );
}

// ---------------------------------------------------------------------------
// (2) Cookie parse: the topmost Record-Route's w_pri/w_bak reach topology.
// ---------------------------------------------------------------------------
#[tokio::test(start_paused = true)]
async fn cookie_parse_sets_topology_pri_bak() {
    let invite = invite_with_cookie("w0", "w1");
    let call = build_initial_call(&invite, src(), &config_for("w0"), 0);
    let topo = call.topology.expect("cookie present → topology set");
    assert_eq!(topo.pri, "w0");
    assert_eq!(topo.bak, "w1", "w_bak reaches topology.bak");
    assert_eq!(topo.gen, 1, "brand-new call starts at gen=1");

    // No cookie (non-proxied INVITE) → topology stays None (legacy flush path).
    let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-nocookie\r\n\
        Max-Forwards: 70\r\n\
        From: <sip:alice@example.com>;tag=alicetag\r\n\
        To: <sip:bob@example.com>\r\n\
        Call-ID: call-nocookie@10.0.0.9\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";
    let req = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Request(r) => r,
        _ => panic!(),
    };
    let call = build_initial_call(&req, src(), &config_for("w0"), 0);
    assert!(call.topology.is_none(), "no cookie → no topology (non-replicating)");
}

// ---------------------------------------------------------------------------
// (3) callGen bump-point: each authoritative CallState::update out-gens the prior.
// ---------------------------------------------------------------------------
#[tokio::test(start_paused = true)]
async fn update_bumps_call_gen() {
    let writer = BufferedTerminateWriter::spawn(
        Arc::new(crate::store::InMemoryCallStore::new()) as Arc<dyn CallStore>,
        16,
    );
    let state = CallState::new(
        Arc::new(crate::store::InMemoryCallStore::new()) as Arc<dyn CallStore>,
        writer,
        "w0",
        B2buaMetrics::new(),
    );

    let invite = invite_with_cookie("w0", "w1");
    let call: Call = build_initial_call(&invite, src(), &config_for("w0"), 0);
    let call_ref = call.call_ref.clone();
    state.create(call.clone());
    assert_eq!(gen(&state, &call_ref), 1, "create keeps the gen=1 baseline");

    state.update(state.peek(&call_ref).unwrap());
    assert_eq!(gen(&state, &call_ref), 2, "first authoritative mutation → gen 2");

    state.update(state.peek(&call_ref).unwrap());
    assert_eq!(gen(&state, &call_ref), 3, "second mutation → gen 3 (monotonic)");
}

fn gen(state: &CallState, call_ref: &str) -> i64 {
    state.peek(call_ref).unwrap().topology.unwrap().gen
}
