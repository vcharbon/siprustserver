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
use repl_net::transport::{RealReplicationNetwork, ReplicationNetwork};
use sip_clock::Clock;
use tokio::sync::watch;

use super::{
    Changelog, FnPeerResolver, Puller, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor,
};
use crate::metrics::B2buaMetrics;
use crate::store::{CallStore, PartitionRole, PropagateDirection, PutOpts};
use topology::{Peer, SimulatedMembership};

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
) -> (watch::Sender<bool>, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let (puller, _status) = Puller::new_at(
        peer_ordinal,
        self_ordinal,
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
    let (_cancel, metrics) = spawn_puller("w1", "w0", w0_addr, &net, &w1);

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
        metrics.repl_pull_applied_total() >= 1,
        "puller applied the replicated entry"
    );
    assert_eq!(metrics.repl_backup_held(), 1, "one backup replica held");
    assert_eq!(
        w1.current_call_gen(BAK, "w0", &call_ref),
        Some(1),
        "replicated at the gen=1 baseline"
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
    let (_cancel, metrics) = spawn_puller("w1", "w0", w0_addr, &net, &w1);

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
            &forward_to("w1"),
        )
        .await
        .unwrap();
    }

    eventually("backup reaches the latest gen", || {
        let w1 = w1.clone();
        let call_ref = call_ref.clone();
        async move { w1.current_call_gen(BAK, "w0", &call_ref) == Some(4) }
    })
    .await;

    let body = w1.get_call(BAK, "w0", &call_ref).await.unwrap().unwrap();
    assert_eq!(&body[..], b"body-v4", "latest update body served");
    assert!(
        metrics.repl_pull_applied_total() >= 1,
        "at least one apply recorded"
    );
    // Compaction: only one live replica for the ref despite four mutations.
    assert_eq!(metrics.repl_backup_held(), 1, "compacted to one live replica");
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
        &PutOpts::default(), // static backup body, NOT in the changelog
    )
    .await
    .unwrap();

    let w1 = ReplicatingCallStore::with_changelog(
        Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000),
        clock.clone(),
    );
    let (_cancel, metrics) = spawn_puller("w1", "w0", w0_addr, &net, &w1);

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
        metrics.repl_pull_applied_total() >= 1,
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
        src.put_call(PRI, primary, &call_ref, format!("body-{i}-v1").into_bytes(), &[], 30_000, 1, &forward_to(peer))
            .await
            .unwrap();
        // In-dialog re-INVITE/UPDATE — second authoritative mutation (gen 2).
        src.put_call(PRI, primary, &call_ref, format!("body-{i}-v2").into_bytes(), &[], 30_000, 2, &forward_to(peer))
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
                if holder.current_call_gen(BAK, primary, &call_ref) != Some(2) {
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
    let (_cancel, metrics) =
        spawn_puller("b2bua-worker-1", "w0", w0_addr, &net, &backup);

    // Bump the changelog under the WRONG peer id ("w1" != the puller's caller).
    let call_ref = "w0|orphaned|tag".to_string();
    w0.put_call(PRI, "w0", &call_ref, b"body".to_vec(), &[], 30_000, 1, &forward_to("w1"))
        .await
        .unwrap();

    // Give the tail ample real time to (not) deliver, then assert zero applies —
    // the silent failure. A bounded sleep is the right tool here: we are proving
    // an absence, so there is no positive event to await.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        metrics.repl_pull_applied_total(),
        0,
        "mismatched changelog peer-key vs puller caller delivers nothing (silent)"
    );
    assert!(
        backup.get_call(BAK, "w0", &call_ref).await.unwrap().is_none(),
        "backup holds no replica under an id mismatch"
    );
}
