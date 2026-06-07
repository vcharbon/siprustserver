//! Real-TCP replication integration tests (goal-3 — the kind/k8s transport).
//!
//! The sim suite (S5–S10) drives the changelog → `ReplServer` → `Puller`
//! protocol over the in-memory `SimulatedReplicationNetwork`, which moves whole
//! `Vec<u8>` frames and pumps a fake clock. That fabric **cannot** exercise the
//! real `RealReplicationNetwork` (tokio `TcpStream` readiness ignores
//! `tokio::time::pause` — Decision X2), so the length-prefix bootstrap/tail
//! streaming over an actual socket was never under test. The live cluster proved
//! the gap real: changelog populated + TCP established, yet `repl_pull_applied =
//! 0`. These tests close it — two `ReplicatingCallStore`s talking over loopback
//! TCP, asserting an in-dialog mutation made on the primary is served to the
//! backup.
//!
//! Real runtime, NOT `start_paused`: real socket I/O does not obey the paused
//! clock. We poll with a bounded real-time timeout instead of `advance`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::Watermark;
use repl_net::transport::{
    RealReplicationNetwork, ReplicationConnection, ReplicationListener, ReplicationNetwork,
};
use sip_clock::Clock;
use tokio::sync::watch;

use super::{
    Changelog, FnPeerResolver, Puller, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor,
};
use crate::metrics::B2buaMetrics;
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};
use topology::{Peer, SimulatedMembership};

// Extra wiring for the end-to-end MATERIALISATION gate: a full `B2buaCore`
// reclaimer (router + reclaim pipeline) over a real SIP endpoint + real repl TCP,
// reclaiming VALID encoded `Call` bodies. The stream gates above stop at "body
// landed in the store"; this reaches "call live + routable + keepalive re-armed".
use std::collections::HashMap;

use call::{CallBodyCodec, MsgpackCodec, TimerEntry, TimerType};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_net::{BindUdpOpts, RealSignalingNetwork, SignalingNetwork};
use sip_txn::IdGen;
use topology::Membership;

use crate::cdr::InMemoryCdrWriter;
use crate::config::B2buaConfig;
use crate::decision::ScriptedDecisionEngine;
use crate::initial_invite::build_initial_call;
use crate::limiter::NoopLimiter;
use crate::store::InMemoryCallStore;
use crate::{B2buaCore, B2buaDeps, ReplicationSetup};

const BAK: PartitionRole = PartitionRole::Backup;
const PRI: PartitionRole = PartitionRole::Primary;

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn fast_config() -> PullerConfig {
    PullerConfig {
        backoff_init_ms: 20,
        backoff_max_ms: 200,
        bootstrap_hard_timeout_ms: 2_000,
    }
}

/// `Forward` flush opts: this node is the primary, `peer` backs it up.
fn forward_to(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

// ── Materialisation-gate helpers ────────────────────────────────────────────
// The stream gates seed garbage `0xAB` bodies — fine, because they only assert
// the body LANDED. The materialisation gate must reclaim them into LIVE calls,
// and `reclaim_scan` decodes every body and silently drops the undecodable ones
// (`store/mod.rs`). So this gate seeds VALID `Call` encodings.

fn src() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 9], 5060))
}

fn config_for(ordinal: &str) -> B2buaConfig {
    B2buaConfig {
        self_ordinal: ordinal.into(),
        ..Default::default()
    }
}

/// A proxied INVITE carrying the `w_pri`/`w_bak` cookie, keyed by Call-ID so each
/// call gets a distinct `callRef` (`{pri}|{cid}@…|alicetag`). Mirrors the
/// `s11_tests` builder so the bodies are byte-identical to a real proxied call.
fn invite(pri: &str, bak: &str, cid: &str) -> SipRequest {
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

/// A valid encoded reclaim body: a real `Call` (`{primary}` primary, `{backup}`
/// backup) carrying a **future-dated** keepalive so the reclaim sweep does real
/// timer-restore work per call without the timer firing (and tearing the call
/// down) mid-measurement. Returns `(call_ref, encoded body)`.
fn reclaim_body(primary: &str, backup: &str, cid: &str, clock: &Clock) -> (String, Vec<u8>) {
    let mut call = build_initial_call(&invite(primary, backup, cid), src(), &config_for(primary), 0);
    call.timers.push(TimerEntry {
        id: format!("keepalive-{cid}"),
        timer_type: TimerType::Keepalive,
        // +5 min: restored into the DelayQueue but never fires during the test.
        fire_at: clock.now_ms() + 300_000,
        leg_id: None,
    });
    let body = MsgpackCodec::new().encode(&call);
    (call.call_ref.clone(), body)
}

/// Spawn a FULL rebooting `B2buaCore` reclaimer (`self_ordinal`, incarnation 2)
/// on a real SIP UDP endpoint, replicating over `repl_net`, pulling its single
/// peer (`peer_ordinal` @ `peer_addr`). Returns the core + its repl store handle.
/// Its supervisor bootstraps the peer and signals the REAL `router::reclaim_all`
/// — so a `serves()` true means the call went all the way through materialise +
/// timer re-arm, not just store-landing.
async fn spawn_reclaimer_core(
    self_ordinal: &str,
    peer_ordinal: &str,
    peer_addr: SocketAddr,
    repl_net: &RealReplicationNetwork,
    clock: &Clock,
) -> (B2buaCore, Arc<ReplicatingCallStore>) {
    let sip_net = RealSignalingNetwork::new();
    let endpoint = sip_net
        .bind_udp(BindUdpOpts::new(loopback(), 256))
        .await
        .expect("bind sip udp");
    let sip_port = endpoint.local_addr().port();

    // Fresh empty store at a higher incarnation gen (the rebooted node).
    let store = Arc::new(ReplicatingCallStore::with_changelog(
        Changelog::new(2, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    ));
    let addr_map: HashMap<String, SocketAddr> =
        [(peer_ordinal.to_string(), peer_addr)].into_iter().collect();
    let addr_resolver: crate::repl::AddrResolver =
        Arc::new(FnPeerResolver(move |p: &Peer| *addr_map.get(&p.ordinal).expect("peer addr")));
    let membership: Arc<dyn Membership> = Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new(peer_ordinal, peer_ordinal)],
        clock.clone(),
    ));
    let setup = ReplicationSetup {
        network: Arc::new(repl_net.clone()),
        membership,
        store: store.clone(),
        listen_addr: loopback(), // ephemeral — nobody pulls this node
        addr_resolver,
        incarnation_gen: 2,
    };
    let config = B2buaConfig {
        self_ordinal: self_ordinal.into(),
        sip_local_ip: "127.0.0.1".into(),
        sip_local_port: sip_port,
        keepalive_interval_sec: 300,
        reboot_budget_sec: 600,
        ..Default::default()
    };
    let deps = B2buaDeps {
        config,
        decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 9)),
        limiter: Arc::new(NoopLimiter),
        cdr: Arc::new(InMemoryCdrWriter::new()),
        store: Arc::new(InMemoryCallStore::new()), // throwaway legacy slot
        clock: clock.clone(),
        id_gen: Arc::new(IdGen::seeded(0xB2B1)),
        replication: Some(setup),
        metrics: crate::metrics::B2buaMetrics::new(),
    };
    (B2buaCore::spawn(endpoint, deps), store)
}

/// Poll `f` until it returns `true` or the real-time budget elapses. Yields a
/// short real sleep between polls so the puller/server background tasks run.
async fn eventually<F, Fut>(label: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if f().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for: {label}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Stand up a primary node `ordinal` (store + changelog + a `ReplServer` running
/// on a real loopback listener) and return its store + bound address.
async fn spawn_primary(
    ordinal: &str,
    net: &RealReplicationNetwork,
    clock: &Clock,
) -> (ReplicatingCallStore, SocketAddr) {
    let changelog = Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000);
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let listener = net.listen(loopback()).await.unwrap();
    let addr = listener.local_addr();
    let server = ReplServer::new(ordinal, changelog, Arc::new(store.clone()));
    tokio::spawn(server.run(listener));
    (store, addr)
}

/// Spawn a puller on the backup node pulling `peer_ordinal` at `peer_addr` into
/// `store`. Returns the cancel handle + the shared metrics it bumps.
fn spawn_puller(
    self_ordinal: &str,
    peer_ordinal: &str,
    peer_addr: SocketAddr,
    net: &RealReplicationNetwork,
    store: &ReplicatingCallStore,
    partition: repl_net::frame::Partition,
) -> (watch::Sender<bool>, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let (puller, _status) = Puller::new_at(
        peer_ordinal,
        self_ordinal,
        partition,
        peer_addr,
        Arc::new(net.clone()) as Arc<dyn ReplicationNetwork>,
        store.clone(),
        fast_config(),
        Watermark::new(0, 0),
        metrics.clone(),
    );
    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(async move { puller.run(cancel_rx).await });
    (cancel_tx, metrics)
}

// ---------------------------------------------------------------------------
// THE goal-3 regression: an in-dialog mutation made on the primary AFTER the
// puller has connected + bootstrapped (cold, empty store) must stream over real
// TCP and land on the backup. This is the path the cluster proved broken
// (`repl_pull_applied = 0`); the sim suite cannot reach it (no real socket).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn tail_delivers_post_connect_mutation_over_real_tcp() {
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    // Primary w0 serves; backup w1 pulls w0 into its store.
    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (_cancel, metrics) =
        spawn_puller("w1", "w0", w0_addr, &net, &w1, repl_net::frame::Partition::Bak);

    // A call is established on the primary AFTER the puller connected (the store
    // was empty at bootstrap time — the tail must carry it). The flush stores
    // the body in w0's pri:w0 and bumps the changelog for peer w1 (Forward).
    let call_ref = "w0|call-realtcp|alicetag".to_string();
    w0.put_call(
        PRI,
        "w0",
        &call_ref,
        b"the-call-body".to_vec(),
        &["idx-a".to_string()],
        30_000,
        1,
        0,
        &forward_to("w1"),
    )
    .await
    .unwrap();

    // The tail must deliver it to w1's bak:w0 over real TCP.
    eventually("backup receives the post-connect call", || {
        let w1 = w1.clone();
        let call_ref = call_ref.clone();
        async move {
            w1.get_call(BAK, "w0", &call_ref)
                .await
                .unwrap()
                .is_some()
        }
    })
    .await;

    assert!(
        metrics.repl_applied_sum() >= 1,
        "puller applied the replicated entry"
    );
    assert_eq!(metrics.repl_backup_replicas(), 1, "one backup replica held");
    assert_eq!(
        w1.current_cv(BAK, "w0", &call_ref),
        Some((1, 0)),
        "replicated at the (p,b)=(1,0) baseline"
    );
    let body = w1.get_call(BAK, "w0", &call_ref).await.unwrap().unwrap();
    assert_eq!(&body[..], b"the-call-body", "body round-trips over TCP");
}

// ---------------------------------------------------------------------------
// A sequence of in-dialog mutations (re-INVITE / UPDATE bumping call_gen) all
// stream and the latest wins (LWW by call_gen). Exercises the steady-state tail
// pushing repeatedly on one long-lived connection.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn tail_streams_successive_updates_over_real_tcp() {
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (_cancel, metrics) =
        spawn_puller("w1", "w0", w0_addr, &net, &w1, repl_net::frame::Partition::Bak);

    let call_ref = "w0|call-updates|tag".to_string();
    for gen in 1..=4i64 {
        let body = format!("body-v{gen}");
        w0.put_call(
            PRI,
            "w0",
            &call_ref,
            body.into_bytes(),
            &[],
            30_000,
            gen,
            0,
            &forward_to("w1"),
        )
        .await
        .unwrap();
    }

    eventually("backup reaches the latest gen", || {
        let w1 = w1.clone();
        let call_ref = call_ref.clone();
        async move { w1.current_cv(BAK, "w0", &call_ref) == Some((4, 0)) }
    })
    .await;

    let body = w1.get_call(BAK, "w0", &call_ref).await.unwrap().unwrap();
    assert_eq!(&body[..], b"body-v4", "latest update body served");
    assert!(
        metrics.repl_applied_sum() >= 1,
        "at least one apply recorded"
    );
    // Compaction: only one live replica for the ref despite four mutations.
    assert_eq!(metrics.repl_backup_replicas(), 1, "compacted to one live replica");
}

// ---------------------------------------------------------------------------
// A backup that was ALREADY holding a call when the puller connects re-hydrates
// it through the cold bootstrap pre-seed (Data(Pri) scan) — the path that runs
// before any tail entry exists. Here the *primary* pre-holds a bak:wX body for
// the caller, so the bootstrap scan has something to stream. Confirms bootstrap
// (not just tail) works over real TCP.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn bootstrap_preseed_delivers_over_real_tcp() {
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    // w0 already backs up a w1-primary call (bak:w1) BEFORE w1's puller connects.
    // On bootstrap, w0 scans bak:w1 and streams it as Data(Pri) so w1 reclaims it.
    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let call_ref = "w1|reclaim-me|tag".to_string();
    w0.put_call(
        BAK,
        "w1",
        &call_ref,
        b"reclaim-body".to_vec(),
        &[],
        30_000,
        7,
        0,
        &PutOpts::default(), // static backup body, NOT in the changelog
    )
    .await
    .unwrap();

    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (_cancel, metrics) =
        spawn_puller("w1", "w0", w0_addr, &net, &w1, repl_net::frame::Partition::Pri);

    // The bootstrap pre-seed imports it into w1's pri:w1 (its own reclaimed call).
    eventually("backup reclaims the pre-seeded call", || {
        let w1 = w1.clone();
        let call_ref = call_ref.clone();
        async move { w1.get_call(PRI, "w1", &call_ref).await.unwrap().is_some() }
    })
    .await;

    let body = w1.get_call(PRI, "w1", &call_ref).await.unwrap().unwrap();
    assert_eq!(&body[..], b"reclaim-body", "pre-seed body round-trips");
    assert!(
        metrics.repl_applied_sum() >= 1,
        "bootstrap pre-seed counted as applied"
    );
}

// ---------------------------------------------------------------------------
// Cluster-faithful: TWO nodes, each running a `ReplServer` AND a
// `ReplicationSupervisor` (driven by `SimulatedMembership`, exactly the runner's
// wiring), backing each other up over real loopback TCP. A burst of DISTINCT
// in-dialog calls — half primaried on w0, half on w1 — must each land on the
// peer's backup partition, and `repl_backup_held` must equal the live replica
// count it serves at takeover. This is the local analog of the chaos-failover
// scenario the sim suite could never reach (no real socket, no two-way push).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn bidirectional_supervisor_replication_over_real_tcp() {
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let (w1, w1_addr) = spawn_primary("w1", &net, &clock).await;

    // Address resolution: ordinal → the node's bound repl addr (the runner's
    // AddrResolver equivalent).
    let addrs: std::collections::HashMap<String, SocketAddr> =
        [("w0".to_string(), w0_addr), ("w1".to_string(), w1_addr)]
            .into_iter()
            .collect();
    let resolve = {
        let addrs = addrs.clone();
        Arc::new(FnPeerResolver(move |peer: &Peer| *addrs.get(&peer.ordinal).unwrap()))
    };

    // Each node's supervisor pulls the OTHER (mutual backup). Membership lists
    // both peers; the supervisor skips self.
    let members = || {
        Arc::new(SimulatedMembership::with_clock(
            vec![Peer::new("w0", "w0"), Peer::new("w1", "w1")],
            clock.clone(),
        ))
    };
    let sup0 = ReplicationSupervisor::with_config(
        "w0",
        Arc::new(net.clone()),
        w0.clone(),
        resolve.clone(),
        clock.clone(),
        fast_config(),
    );
    let sup1 = ReplicationSupervisor::with_config(
        "w1",
        Arc::new(net.clone()),
        w1.clone(),
        resolve.clone(),
        clock.clone(),
        fast_config(),
    );
    sup0.start(members());
    sup1.start(members());

    // Both pullers reach steady-state (terminal bootstrap Noop seen).
    eventually("w0 current on w1 and w1 current on w0", || {
        let (sup0, sup1) = (sup0.clone(), sup1.clone());
        async move { sup0.is_current("w1") && sup1.is_current("w0") }
    })
    .await;

    // A burst of distinct calls: even refs primaried on w0 (→ backed up on w1),
    // odd refs primaried on w1 (→ backed up on w0). Each is created once, so its
    // changelog entry stays `Create`; an in-dialog UPDATE then bumps it to gen 2
    // and compacts the entry to `Update` before the peer necessarily drains.
    for i in 0..6 {
        let (src, primary, peer) = if i % 2 == 0 {
            (&w0, "w0", "w1")
        } else {
            (&w1, "w1", "w0")
        };
        let call_ref = format!("{primary}|burst-{i}|tag");
        src.put_call(PRI, primary, &call_ref, format!("body-{i}-v1").into_bytes(), &[], 30_000, 1, 0, &forward_to(peer))
            .await
            .unwrap();
        // In-dialog re-INVITE/UPDATE — second authoritative mutation (gen 2).
        src.put_call(PRI, primary, &call_ref, format!("body-{i}-v2").into_bytes(), &[], 30_000, 2, 0, &forward_to(peer))
            .await
            .unwrap();
    }

    // Every backed-up call must reach the peer at gen 2 (latest), on the right
    // backup partition, regardless of which side primaried it.
    eventually("all 6 calls replicated to their backups at gen 2", || {
        let (w0, w1) = (w0.clone(), w1.clone());
        async move {
            for i in 0..6 {
                let (holder, primary) = if i % 2 == 0 { (&w1, "w0") } else { (&w0, "w1") };
                let call_ref = format!("{primary}|burst-{i}|tag");
                if holder.current_cv(BAK, primary, &call_ref) != Some((2, 0)) {
                    return false;
                }
            }
            true
        }
    })
    .await;

    // Body integrity on a sampled replica from each direction.
    let on_w1 = w1.get_call(BAK, "w0", "w0|burst-0|tag").await.unwrap().unwrap();
    assert_eq!(&on_w1[..], b"body-0-v2", "w0→w1 replica carries the latest body");
    let on_w0 = w0.get_call(BAK, "w1", "w1|burst-1|tag").await.unwrap().unwrap();
    assert_eq!(&on_w0[..], b"body-1-v2", "w1→w0 replica carries the latest body");

    sup0.shutdown();
    sup1.shutdown();
}

// ---------------------------------------------------------------------------
// THROUGHPUT floor (real TCP + real clock): a cold node must be able to
// re-hydrate (synchronise) its backup partition at MORE THAN 5 000 contexts per
// second. This is the perf counterpart to the correctness fix for the
// ~203/3000 truncation: there we proved a large bootstrap completes IN FULL
// regardless of how long it takes (the per-frame idle timer); here we prove it
// also completes FAST ENOUGH over a real socket on a real clock. `start_paused`
// cannot measure this — real `TcpStream` readiness ignores the fake clock — so
// this rides loopback TCP and wall time, mirroring the cluster path.
//
// Shape = the reboot-reclaim bulk: the primary pre-holds a large static
// `bak:{w1}` keyset (peer:None ⇒ NOT in the changelog, so the bootstrap scan is
// the SOLE delivery path), then a cold w1 bootstraps the whole set. We assert
// (a) every context lands (completeness at scale) and (b) the sustained sync
// rate clears the 5 000 ctx/s floor.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn bootstrap_synchronises_above_5k_contexts_per_second_over_real_tcp() {
    const N: usize = 5_000;
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    // w0 holds N static backup bodies for w1 (the bulk a rebooted w1 reclaims).
    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let body = vec![0xABu8; 256]; // a representative ~256-byte call context
    for i in 0..N {
        let call_ref = format!("w1|sync-{i}|tag");
        w0.put_call(
            BAK,
            "w1",
            &call_ref,
            body.clone(),
            &[format!("idx-{i}")],
            300_000,
            1,
            0,
            &PutOpts::default(), // static backup body, NOT in the changelog
        )
        .await
        .unwrap();
    }

    // Cold w1 bootstraps the full set; time from connect to last context landed.
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let started = tokio::time::Instant::now();
    let (_cancel, metrics) =
        spawn_puller("w1", "w0", w0_addr, &net, &w1, repl_net::frame::Partition::Pri);

    // Poll until all N have re-hydrated into pri:w1, capturing wall time. The
    // 30s ceiling is a correctness backstop only (so a real regression fails
    // instead of hanging); the throughput assertion below is the perf gate.
    let deadline = started + Duration::from_secs(30);
    loop {
        if w1.scan_call_refs(PRI, "w1").len() >= N {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "only {}/{N} contexts re-hydrated within 30s",
                w1.scan_call_refs(PRI, "w1").len()
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let elapsed = started.elapsed();

    // (a) Completeness: every context present, none lost to a watermark
    //     collision or a truncated stream.
    assert_eq!(
        w1.scan_call_refs(PRI, "w1").len(),
        N,
        "all {N} contexts re-hydrated into pri:w1"
    );
    assert!(
        metrics.repl_applied_sum() >= N as u64,
        "every context counted as applied ({} < {N})",
        metrics.repl_applied_sum(),
    );

    // (b) Throughput floor: > 5 000 contexts/second sustained over real TCP.
    //     (Conservative — `elapsed` includes the poll-sleep slack, understating
    //     the true rate.)
    let rate = N as f64 / elapsed.as_secs_f64();
    assert!(
        rate > 5_000.0,
        "bootstrap sync throughput {rate:.0} ctx/s is below the 5 000 ctx/s floor \
         ({N} contexts in {elapsed:?})",
    );
}

// ---------------------------------------------------------------------------
// MATERIALISATION gate (real `reclaim_all`, end-to-end, under serving load).
//
// The stream gates above stop one stage too early: they assert the body LANDED
// in the store (`scan_call_refs` / `repl_pull_applied`). A call only becomes
// LIVE + routable + keepalive-armed after the SECOND stage —
// `ReclaimAll → reclaim_into_live → materialize_if_absent + timers.restore`
// (`router.rs`), which a bare `ReplicatingCallStore` can't reach. That second
// stage is exactly what was slow on the live cluster (`repl_reclaimed_total`
// trickled 203→433 over 15 min at ~30% CPU — wait-bound, not compute-bound), so
// a throughput gate that finishes at store-landing can never see it.
//
// This gate runs a FULL rebooting `B2buaCore` (router + reclaim pipeline) over
// real repl TCP, reclaiming VALID `Call` bodies, and finishes only when the node
// `serves()` every one — measuring the stage the stream gates skip. It runs on
// `multi_thread` with a live-write storm contending the reclaimer's store Mutex
// (finding #6 applied to the reclaim side) so the materialise pipeline is timed
// under the load a real reboot reclaims under.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaim_materialises_into_live_map_under_serving_load() {
    use std::sync::atomic::{AtomicBool, Ordering};

    const N: usize = 3_000; // the production keyset size
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    // w0: a bare peer holding N of w1's calls as static `bak:w1` replicas — valid
    // encoded `Call` bodies (peer:None ⇒ not in the changelog, so the bootstrap
    // scan is the SOLE delivery path: the reboot-reclaim bulk).
    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;
    let mut refs = Vec::with_capacity(N);
    for i in 0..N {
        let (call_ref, body) = reclaim_body("w1", "w0", &format!("cid-{i}"), &clock);
        w0.put_call(BAK, "w1", &call_ref, body, &[format!("idx-{i}")], 300_000, 1, 0, &PutOpts::default())
            .await
            .unwrap();
        refs.push(call_ref);
    }

    // w1: a FULL rebooting B2buaCore reclaimer pointed at w0. Start the clock the
    // instant it comes up — the measured interval is connect → bootstrap → the
    // real reclaim_all materialising every call.
    let started = tokio::time::Instant::now();
    let (w1, w1_store) = spawn_reclaimer_core("w1", "w0", w0_addr, &net, &clock).await;

    // Concurrent serving load: hammer w1's store with backup forward-flushes while
    // it reclaims. These hit the SAME inner Mutex as `reclaim_scan` + the bootstrap
    // applies (finding #6) but land in `bak:w9` — NOT scanned by reclaim (which
    // scans `pri:w1`), so they pressure the lock without polluting the count.
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..3usize {
        let (store, stop) = (w1_store.clone(), stop.clone());
        writers.push(tokio::spawn(async move {
            let live = vec![0xCDu8; 256];
            let mut j = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let cr = format!("w9|bakload-{w}-{j}|t");
                let _ = store
                    .put_call(BAK, "w9", &cr, live.clone(), &[], 300_000, 1, 0, &PutOpts::default())
                    .await;
                j += 1;
                if j % 64 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    // Finish line = SERVES (materialised + routable + keepalive re-armed) every
    // reclaimed call. A reactive-trickle / truncation regression never gets here
    // (no inbound traffic drives the stragglers) ⇒ the 30s backstop fails loudly.
    let deadline = started + Duration::from_secs(30);
    loop {
        if refs.iter().all(|r| w1.serves(r)) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let served = refs.iter().filter(|r| w1.serves(r)).count();
            panic!(
                "only {served}/{N} reclaimed calls reached the LIVE serving map within 30s \
                 (reclaimed_total={}, active={}) — store-landing is not enough",
                w1.metrics().repl_reclaimed_total(),
                w1.active_calls(),
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for h in writers {
        let _ = h.await;
    }

    // (a) Completeness — the live-cluster failure was `scanned ≫ materialised`
    //     (203/3000). Assert the REAL bulk sweep materialised every call: ground
    //     truth live count == N, and the cumulative fresh-materialise counter == N
    //     (idempotent re-passes add nothing, so this stays exact).
    assert_eq!(w1.active_calls(), N, "every reclaimed call is live in the serving map");
    assert_eq!(
        w1.metrics().repl_reclaimed_total(),
        N as u64,
        "the bulk reclaim_all materialised all {N} (no reactive trickle, no truncation)",
    );

    // (b) Materialisation throughput floor under serving load — comfortably met
    //     over loopback; its job is to fail if the materialise stage regresses to
    //     a slow drip the 30s backstop would otherwise mask.
    let rate = N as f64 / elapsed.as_secs_f64();
    eprintln!(
        "\n=== reclaim materialisation (N={N}, real TCP, multi_thread, under serving load) ===\n  \
         connect→bootstrap→serves-all: {rate:>9.0} calls/s ({N} in {elapsed:?})\n",
    );
    assert!(
        rate > 1_000.0,
        "reclaim materialisation {rate:.0} calls/s under serving load is below the \
         1 000 calls/s floor ({N} in {elapsed:?})",
    );
}

// ---------------------------------------------------------------------------
// Identity-key invariant (the cluster's *actual* `repl_pull_applied = 0`):
// the server drains a peer's changelog keyed by the PULLER's `caller` ordinal,
// which the changelog was bumped under as `opts.peer` (= the proxy cookie's
// `w_bak`, echoed onto `topology.bak`). If the changelog is bumped under one id
// ("w1") but the puller connects as a DIFFERENT id ("b2bua-worker-1"), the
// server finds no entries for that caller and silently delivers NOTHING — no
// error, no close, just zero applies. This is what bit the live cluster:
// `run.sh` keyed the proxy registry as `w${i}` while each worker's ordinal was
// its pod name, so the keys never matched. This test pins that silent-failure
// contract so the invariant ("changelog peer-key == puller caller") is explicit;
// the deploy fix (pod-name proxy ids) keeps them equal by construction.
#[tokio::test]
async fn mismatched_ordinal_silently_delivers_nothing() {
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    let (w0, w0_addr) = spawn_primary("w0", &net, &clock).await;

    // The puller connects as "b2bua-worker-1" — but the primary will bump its
    // changelog for peer "w1" (the proxy's mismatched cookie id).
    let backup = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (_cancel, metrics) = spawn_puller(
        "b2bua-worker-1",
        "w0",
        w0_addr,
        &net,
        &backup,
        repl_net::frame::Partition::Bak,
    );

    // Bump the changelog under the WRONG peer id ("w1" != the puller's caller).
    let call_ref = "w0|orphaned|tag".to_string();
    w0.put_call(PRI, "w0", &call_ref, b"body".to_vec(), &[], 30_000, 1, 0, &forward_to("w1"))
        .await
        .unwrap();

    // Give the tail ample real time to (not) deliver, then assert zero applies —
    // the silent failure. A bounded sleep is the right tool here: we are proving
    // an absence, so there is no positive event to await.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        metrics.repl_applied_sum(),
        0,
        "mismatched changelog peer-key vs puller caller delivers nothing (silent)"
    );
    assert!(
        backup.get_call(BAK, "w0", &call_ref).await.unwrap().is_none(),
        "backup holds no replica under an id mismatch"
    );
}

// ---------------------------------------------------------------------------
// THROUGHPUT BENCH (manual; `--ignored`). Measures bootstrap re-hydration
// ctx/s over real TCP, on a MULTI-THREAD runtime, with and without a concurrent
// live-write storm on the SERVER — the only setup that exercises the real
// bottleneck (finding #6: the single `meta` Mutex shared by serve_bootstrap's
// per-body reads and live put_call/delete_call). On a current-thread runtime a
// std Mutex is never contended (one task at a time, no lock held across await),
// so the plain CI test cannot represent #6 — this bench is the representative
// measurement that justifies (or refutes) a perf fix. Writers forward to a
// dummy "w2" so they load w0's locks WITHOUT polluting w1's pri: count.
//
// Run: `cargo test -p b2bua --release repl::real_transport_tests::bench -- --ignored --nocapture`
// ---------------------------------------------------------------------------
async fn measure_bootstrap(
    net: &RealReplicationNetwork,
    clock: &Clock,
    n: usize,
    writers: usize,
) -> (Duration, u64, u64) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let (w0, w0_addr) = spawn_primary("w0", net, clock).await;
    let body = vec![0xABu8; 256];
    for i in 0..n {
        let cr = format!("w1|sync-{i}|tag");
        w0.put_call(BAK, "w1", &cr, body.clone(), &[], 300_000, 1, 0, &PutOpts::default())
            .await
            .unwrap();
    }
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );

    // Concurrent live-write storm on w0 (forwarded to a dummy peer so it never
    // reaches w1) — pure contention on w0's meta/inner locks during the scan.
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let (w0, stop, writes) = (w0.clone(), stop.clone(), writes.clone());
        handles.push(tokio::spawn(async move {
            let live = vec![0xCDu8; 256];
            let mut j = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let cr = format!("w0|live-{w}-{j}|tag");
                let _ = w0
                    .put_call(PRI, "w0", &cr, live.clone(), &[], 300_000, 1, 0, &forward_to("w2"))
                    .await;
                j += 1;
                writes.fetch_add(1, Ordering::Relaxed);
                if j % 64 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    let started = tokio::time::Instant::now();
    let (_cancel, metrics) =
        spawn_puller("w1", "w0", w0_addr, net, &w1, repl_net::frame::Partition::Pri);
    let deadline = started + Duration::from_secs(120);
    loop {
        if w1.scan_call_refs(PRI, "w1").len() >= n {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("bench timed out: {}/{n}", w1.scan_call_refs(PRI, "w1").len());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.await;
    }
    (elapsed, metrics.repl_applied_sum(), writes.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// STREAM gate UNDER CONCURRENT SERVING LOAD (the promoted contention bench).
// `bootstrap_synchronises_above_5k_…` measures a cold node in ISOLATION. A real
// rebooted worker re-hydrates WHILE serving ~100 cps of new traffic, and
// `serve_bootstrap`'s per-body reads share the server's single `meta` Mutex with
// live `put_call`/`delete_call` (finding #6). On a current-thread runtime a std
// Mutex is never contended (one task at a time), so a plain CI test cannot
// represent #6 — this rides `multi_thread` + a live-write storm, and GATES that
// the contended bootstrap still lands every context AND clears the same 5 000
// ctx/s floor (measured #6 cost is ~4x, leaving ~14x of headroom). Writers
// forward to a dummy "w2" so they load w0's locks WITHOUT polluting w1's pri:.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_throughput_survives_live_write_contention() {
    const N: usize = 5_000;
    const WRITERS: usize = 3;
    let clock = Clock::test_at(0);
    let net = RealReplicationNetwork::new();

    let (e0, a0, _) = measure_bootstrap(&net, &clock, N, 0).await;
    let (ec, ac, wc) = measure_bootstrap(&net, &clock, N, WRITERS).await;

    let r0 = N as f64 / e0.as_secs_f64();
    let rc = N as f64 / ec.as_secs_f64();
    eprintln!("\n=== bootstrap re-hydration throughput (N={N}, real TCP, multi_thread) ===");
    eprintln!("  no contention      : {r0:>9.0} ctx/s   ({N} in {e0:?}, applied={a0})");
    eprintln!("  contention x{WRITERS}      : {rc:>9.0} ctx/s   ({N} in {ec:?}, applied={ac})");
    eprintln!("  server absorbed {wc} concurrent live writes during the contended run");
    eprintln!("  slowdown factor    : {:.2}x\n", r0 / rc);

    // (a) Completeness is the primary signal: contention must not TRUNCATE the
    //     stream (every context still applies, both runs).
    assert!(a0 >= N as u64, "uncontended run applied all {N} ({a0})");
    assert!(
        ac >= N as u64,
        "contended run lost contexts — applied {ac}/{N} under a live-write storm",
    );
    assert!(wc > 0, "the contended run must actually exert write pressure");

    // (b) Throughput floor UNDER load: the same 5 000 ctx/s bar the isolated
    //     gate holds. A per-frame-flush regression (memory: ~1 000 ctx/s) or a
    //     lock held across the socket would breach it; ~14x headroom otherwise.
    assert!(
        rc > 5_000.0,
        "contended bootstrap {rc:.0} ctx/s is below the 5 000 ctx/s floor \
         ({N} in {ec:?}) — serving load collapsed re-hydration",
    );
}

// ---------------------------------------------------------------------------
// LATENCY-INJECTING wrapper: adds a fixed per-`send` cost to model a real
// network / a CPU-loaded send path (loopback's per-send cost is ~0, which hides
// finding #4 — sequential one-await-send-per-body with no pipelining). Wraps
// BOTH ends so serve_bootstrap's server-side sends pay the cost.
// ---------------------------------------------------------------------------
struct LatentNet {
    inner: Arc<dyn ReplicationNetwork>,
    send_cost: Duration,
}
struct LatentListener {
    inner: Box<dyn repl_net::transport::ReplicationListener>,
    send_cost: Duration,
}
struct LatentConn {
    inner: Box<dyn ReplicationConnection>,
    send_cost: Duration,
}

#[async_trait::async_trait]
impl ReplicationNetwork for LatentNet {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, repl_net::transport::ConnectError> {
        let inner = self.inner.connect(dst).await?;
        Ok(Box::new(LatentConn { inner, send_cost: self.send_cost }))
    }
    async fn listen(
        &self,
        local: SocketAddr,
    ) -> Result<Box<dyn repl_net::transport::ReplicationListener>, repl_net::transport::ListenError>
    {
        let inner = self.inner.listen(local).await?;
        Ok(Box::new(LatentListener { inner, send_cost: self.send_cost }))
    }
}
#[async_trait::async_trait]
impl repl_net::transport::ReplicationListener for LatentListener {
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>> {
        let inner = self.inner.accept().await?;
        Some(Box::new(LatentConn { inner, send_cost: self.send_cost }))
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}
#[async_trait::async_trait]
impl ReplicationConnection for LatentConn {
    async fn send(&self, frame: repl_net::Frame) -> Result<(), repl_net::transport::SendError> {
        tokio::time::sleep(self.send_cost).await;
        self.inner.send(frame).await
    }
    async fn send_batch(
        &self,
        frames: Vec<repl_net::Frame>,
    ) -> Result<(), repl_net::transport::SendError> {
        // One coalesced write/flush ⇒ one network round, regardless of how many
        // frames it carries (the whole point of the batch).
        tokio::time::sleep(self.send_cost).await;
        self.inner.send_batch(frames).await
    }
    async fn recv(&self) -> Option<repl_net::Frame> {
        self.inner.recv().await
    }
    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

/// Measure a cold bootstrap of `n` bodies over a transport with a fixed
/// per-send cost. Returns wall time.
async fn measure_bootstrap_latent(
    base: &RealReplicationNetwork,
    clock: &Clock,
    n: usize,
    send_cost: Duration,
) -> Duration {
    let net = LatentNet {
        inner: Arc::new(base.clone()),
        send_cost,
    };
    let changelog = Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000);
    let w0 = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let listener = net.listen(loopback()).await.unwrap();
    let w0_addr = listener.local_addr();
    tokio::spawn(ReplServer::new("w0", changelog, Arc::new(w0.clone())).run(listener));
    let body = vec![0xABu8; 256];
    for i in 0..n {
        let cr = format!("w1|sync-{i}|tag");
        w0.put_call(BAK, "w1", &cr, body.clone(), &[], 300_000, 1, 0, &PutOpts::default())
            .await
            .unwrap();
    }
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let metrics = B2buaMetrics::new();
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        repl_net::frame::Partition::Pri,
        w0_addr,
        Arc::new(net) as Arc<dyn ReplicationNetwork>,
        w1.clone(),
        fast_config(),
        Watermark::new(0, 0),
        metrics,
    );
    let (_cancel, cancel_rx) = watch::channel(false);
    tokio::spawn(async move { puller.run(cancel_rx).await });

    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(120);
    loop {
        if w1.scan_call_refs(PRI, "w1").len() >= n {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("latent bench timed out: {}/{n}", w1.scan_call_refs(PRI, "w1").len());
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    started.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_bootstrap_throughput_vs_send_latency() {
    const N: usize = 3_000; // the production keyset size
    let clock = Clock::test_at(0);
    let base = RealReplicationNetwork::new();
    eprintln!("\n=== bootstrap re-hydration vs per-send cost (N={N}, real TCP) ===");
    for us in [0u64, 50, 100, 200, 500, 1000] {
        let cost = Duration::from_micros(us);
        let e = measure_bootstrap_latent(&base, &clock, N, cost).await;
        let rate = N as f64 / e.as_secs_f64();
        eprintln!("  send_cost {us:>4}us : {rate:>9.0} ctx/s   ({N} in {e:?})");
    }
    eprintln!();
}

// ---------------------------------------------------------------------------
// ROUND-COUNTING wrapper: tallies network rounds (each `send` / `send_batch` =
// one write+flush = one round). Lets a CI test assert the STRUCTURAL property
// the latency bench proved necessary — a bootstrap of N bodies costs
// O(N/chunk) rounds, not O(N) — deterministically, with no wall-clock.
// ---------------------------------------------------------------------------
struct CountingNet {
    inner: Arc<dyn ReplicationNetwork>,
    rounds: Arc<std::sync::atomic::AtomicU64>,
}
struct CountingListener {
    inner: Box<dyn ReplicationListener>,
    rounds: Arc<std::sync::atomic::AtomicU64>,
}
struct CountingConn {
    inner: Box<dyn ReplicationConnection>,
    rounds: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait::async_trait]
impl ReplicationNetwork for CountingNet {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, repl_net::transport::ConnectError> {
        let inner = self.inner.connect(dst).await?;
        Ok(Box::new(CountingConn { inner, rounds: self.rounds.clone() }))
    }
    async fn listen(
        &self,
        local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, repl_net::transport::ListenError> {
        let inner = self.inner.listen(local).await?;
        Ok(Box::new(CountingListener { inner, rounds: self.rounds.clone() }))
    }
}
#[async_trait::async_trait]
impl ReplicationListener for CountingListener {
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>> {
        let inner = self.inner.accept().await?;
        Some(Box::new(CountingConn { inner, rounds: self.rounds.clone() }))
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}
#[async_trait::async_trait]
impl ReplicationConnection for CountingConn {
    async fn send(&self, frame: repl_net::Frame) -> Result<(), repl_net::transport::SendError> {
        self.rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.send(frame).await
    }
    async fn send_batch(
        &self,
        frames: Vec<repl_net::Frame>,
    ) -> Result<(), repl_net::transport::SendError> {
        self.rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.send_batch(frames).await
    }
    async fn recv(&self) -> Option<repl_net::Frame> {
        self.inner.recv().await
    }
    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

// Representative CI gate for finding #4 (deterministic, no wall-clock). The
// latency bench proves a per-frame flush collapses bootstrap to ~1/send_cost on
// any real link; batching is what keeps it above the 5k ctx/s floor. This pins
// the structural property that guarantees it: a cold bootstrap of N bodies must
// coalesce into O(N/chunk) network rounds, NOT O(N). Pre-fix (one send+flush
// per body) the server alone emitted N rounds; with batching it emits
// ~ceil(N/chunk) + the handful of control frames. Asserting a tight round
// budget fails loudly if anyone reverts to per-frame sends.
#[tokio::test]
async fn bootstrap_coalesces_into_few_network_rounds() {
    use std::sync::atomic::{AtomicU64, Ordering};
    const N: usize = 2_000;
    let clock = Clock::test_at(0);
    let rounds = Arc::new(AtomicU64::new(0));
    let net = CountingNet {
        inner: Arc::new(RealReplicationNetwork::new()),
        rounds: rounds.clone(),
    };

    // Primary w0 holds N static bak:w1 bodies; serve over the counting net.
    let changelog = Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000);
    let w0 = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let listener = net.listen(loopback()).await.unwrap();
    let w0_addr = listener.local_addr();
    tokio::spawn(ReplServer::new("w0", changelog, Arc::new(w0.clone())).run(listener));
    let body = vec![0xABu8; 256];
    for i in 0..N {
        let cr = format!("w1|sync-{i}|tag");
        w0.put_call(BAK, "w1", &cr, body.clone(), &[], 300_000, 1, 0, &PutOpts::default())
            .await
            .unwrap();
    }

    // Cold w1 bootstraps over the counting net.
    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (puller, _status) = Puller::new_at(
        "w0",
        "w1",
        repl_net::frame::Partition::Pri,
        w0_addr,
        Arc::new(net) as Arc<dyn ReplicationNetwork>,
        w1.clone(),
        fast_config(),
        Watermark::new(0, 0),
        B2buaMetrics::new(),
    );
    let (_cancel, cancel_rx) = watch::channel(false);
    tokio::spawn(async move { puller.run(cancel_rx).await });

    // Wait for full re-hydration.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if w1.scan_call_refs(PRI, "w1").len() >= N {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "bootstrap did not complete");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // CHUNK is 128 ⇒ ceil(2000/128) = 16 batched body-rounds, plus the terminal
    // Noop, the single PullRequest, and the steady-state Noop(s): ~20 total. Budget
    // 64 leaves generous headroom for control frames yet is ~30x below the 2000
    // a per-frame-flush regression would emit.
    let total = rounds.load(Ordering::Relaxed);
    assert!(
        total < 64,
        "bootstrap of {N} bodies used {total} network rounds — expected O(N/chunk) (~20); \
         a per-frame send+flush regression would use ~{N}. Batching is not in effect.",
    );
}
