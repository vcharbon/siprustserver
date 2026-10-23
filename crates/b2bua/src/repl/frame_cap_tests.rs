//! A body past the frame cap is refused by the transport unsent: the server
//! logs it, counts it and skips it, and the flow it rides goes on — in the
//! poll tail and in the bootstrap scan alike.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::test_support::{cref, fast_config, fwd, supervisor_for, tick};
use super::{Changelog, ReplServer, ReplicatingCallStore};
use crate::metrics::B2buaMetrics;
use crate::store::{CallStore, PartitionRole};

const PRI: PartitionRole = PartitionRole::Primary;
const BAK: PartitionRole = PartitionRole::Backup;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9940 + n))
}

/// A body one byte past what a frame may carry.
fn oversized() -> Vec<u8> {
    vec![0; repl_net::MAX_FRAME_LEN as usize + 1]
}

/// Node A serving its changelog with its metrics on `net`.
async fn primary(
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
) -> (ReplicatingCallStore, B2buaMetrics) {
    let changelog = Changelog::new(1, clock.clone()).with_ttls(30_000, 300_000);
    let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
    let metrics = B2buaMetrics::new();
    let listener = net.listen(addr(1)).await.unwrap();
    let server =
        ReplServer::new("A", changelog, Arc::new(store.clone())).with_metrics(metrics.clone());
    tokio::spawn(server.run(listener));
    (store, metrics)
}

/// Put A's call `id` with `body`, backed up by B.
async fn put(store: &ReplicatingCallStore, id: &str, body: Vec<u8>) {
    store.put_call(PRI, "A", &cref("A", id), body, &[], 0, 1, 0, &fwd("B")).await.unwrap();
}

/// Whether B holds A's call `id`.
async fn held(store: &ReplicatingCallStore, id: &str) -> bool {
    store.get_call(BAK, "A", &cref("A", id)).await.unwrap().is_some()
}

/// B, pulling A.
fn backup(net: &Arc<SimulatedReplicationNetwork>, clock: &Clock) -> ReplicatingCallStore {
    let store = ReplicatingCallStore::new(2, clock.clone());
    let sup = supervisor_for("B", &store, net, clock, vec![("A".into(), addr(1))], fast_config());
    sup.start(Arc::new(SimulatedMembership::with_clock(vec![Peer::new("A", "A")], clock.clone())));
    store
}

#[tokio::test(start_paused = true)]
async fn an_oversized_frame_in_the_tail_is_counted_and_skipped() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, metrics) = primary(&net, &clock).await;
    let b = backup(&net, &clock);
    tick(100).await;

    put(&a, "before", b"before".to_vec()).await;
    put(&a, "huge", oversized()).await;
    put(&a, "after", b"after".to_vec()).await;
    tick(200).await;

    assert!(held(&b, "before").await, "the call before the refused frame arrives");
    assert!(!held(&b, "huge").await, "the refused frame never left");
    assert!(held(&b, "after").await, "the stream went on past it");
    assert_eq!(metrics.repl_frames_refused_total(), 1, "the refusal is counted");
    put(&a, "later", b"later".to_vec()).await;
    tick(200).await;
    assert!(held(&b, "later").await, "the flow still streams");
}

#[tokio::test(start_paused = true)]
async fn an_oversized_frame_in_the_bootstrap_is_counted_and_skipped() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (a, metrics) = primary(&net, &clock).await;
    put(&a, "before", b"before".to_vec()).await;
    put(&a, "huge", oversized()).await;
    put(&a, "after", b"after".to_vec()).await;

    let b = backup(&net, &clock);
    tick(500).await;

    assert!(held(&b, "before").await, "the bootstrap carries the call before");
    assert!(!held(&b, "huge").await, "the refused frame never left");
    assert!(held(&b, "after").await, "the bootstrap went on past it");
    assert_eq!(metrics.repl_frames_refused_total(), 1, "the refusal is counted once");
    put(&a, "later", b"later".to_vec()).await;
    tick(200).await;
    assert!(held(&b, "later").await, "the tail streams after the bootstrap");
}
