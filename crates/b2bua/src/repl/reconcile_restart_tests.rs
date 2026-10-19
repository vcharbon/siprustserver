//! A membership reconcile whose snapshot panics on every pass is restarted
//! and counted on every run: the supervisor outlives any number of panics.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::broadcast;
use topology::{MemberDelta, Membership, Peer, SimulatedMembership};

use super::test_support::{fast_config, supervisor_for, tick};
use super::ReplicatingCallStore;
use crate::supervise::{MEMBERSHIP_RECONCILE, RESTARTS};

/// A membership whose snapshot panics once `broken` is set.
struct Breaks {
    inner: SimulatedMembership,
    broken: AtomicBool,
}

impl Membership for Breaks {
    fn snapshot(&self) -> Vec<Peer> {
        assert!(!self.broken.load(Ordering::SeqCst), "the snapshot source is broken");
        self.inner.snapshot()
    }

    fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        self.inner.changes()
    }
}

#[tokio::test(start_paused = true)]
async fn a_reconcile_that_panics_on_every_snapshot_is_restarted_every_time() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let store = ReplicatingCallStore::new(6, clock.clone());
    let sup = supervisor_for("A", &store, &net, &clock, Vec::new(), fast_config());
    let membership = Arc::new(Breaks {
        inner: SimulatedMembership::with_clock(Vec::new(), clock.clone()),
        broken: AtomicBool::new(false),
    });
    sup.start(membership.clone() as Arc<dyn Membership>);
    tick(500).await; // the peerless backup gate latches and ends

    let before = RESTARTS.count(MEMBERSHIP_RECONCILE);
    membership.broken.store(true, Ordering::SeqCst);
    // The 5 s tick panics the follower; every restart reconciles at once and
    // panics again, one restart per second.
    tick(8_000).await;
    let counted = RESTARTS.count(MEMBERSHIP_RECONCILE) - before;
    assert!(counted >= 2, "each panicking run is restarted and counted, got {counted}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        RESTARTS.count(MEMBERSHIP_RECONCILE) - before > counted,
        "the supervisor still runs after {counted} panics"
    );
    sup.shutdown();
}
