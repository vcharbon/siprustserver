//! A puller whose run panics is respawned by the supervisor's next reconcile
//! and counted in `b2bua_task_restarts_total{task="repl_puller"}`; the flow it
//! serves catches up as if it had never stopped.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Membership, Peer};

use super::test_support::{fast_config, one_peer, tick, Node};
use super::{FnPeerResolver, ReplicatingCallStore, ReplicationSupervisor};
use crate::supervise::{REPL_PULLER, RESTARTS};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9990 + n))
}

#[tokio::test(start_paused = true)]
async fn a_puller_that_panics_is_respawned_and_counted() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(1);
    let _peer = Node::spawn("B", b, 5, &net, &clock).await;

    let store = ReplicatingCallStore::new(6, clock.clone());
    let first = Arc::new(AtomicBool::new(true));
    let resolve = Arc::new(FnPeerResolver(move |_: &Peer| {
        assert!(!first.swap(false, Ordering::SeqCst), "the first resolve panics its puller");
        b
    }));
    let sup = ReplicationSupervisor::with_config("A", net.clone(), store, resolve, fast_config());
    let before = RESTARTS.count(REPL_PULLER);
    sup.start(one_peer("B", &clock) as Arc<dyn Membership>);

    tick(500).await;
    assert!(!sup.is_current("B"), "the first puller panicked before it connected");

    // The reconcile period is 5 s: the next pass finds the ended puller.
    tick(6_000).await;
    assert!(sup.is_current("B"), "the respawned puller caught up");
    assert_eq!(RESTARTS.count(REPL_PULLER), before + 1, "the respawn is counted once");
    sup.shutdown();
}
