//! A membership view that has not synced holds readiness only until the
//! bootstrap hard timer: then the node goes Ready with the view it has, and
//! the peers join as the view fills.
//!
//! ```text
//!   t0         wired, the informer has not listed: no peer, not authoritative
//!   < 2 s      NotReady — an empty view may still hide peers to reclaim from
//!   2 s        the hard timer fires: Ready with the (empty) view
//!   later      the informer lists B: B's flows start, B goes current
//! ```

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Membership, Peer, SimulatedMembership};

use super::test_support::{fast_config, supervisor_for, tick, Node};
use super::{Readiness, ReadinessState, ReplicatingCallStore};

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9980 + n))
}

#[tokio::test(start_paused = true)]
async fn an_unsynced_membership_holds_readiness_only_until_the_bootstrap_hard_timer() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = addr(1);
    let _peer = Node::spawn("B", b, 5, &net, &clock).await;

    let store = ReplicatingCallStore::new(6, clock.clone());
    let sup = supervisor_for("A", &store, &net, &clock, vec![("B".into(), b)], fast_config());
    let membership = Arc::new(SimulatedMembership::unsynced(clock.clone()));
    sup.start(membership.clone() as Arc<dyn Membership>);
    let readiness = Readiness::new(Arc::new(sup.clone()));

    // `tick` advances one trailing chunk past its span: 1.9 s.
    tick(1_800).await;
    assert_eq!(
        readiness.state(),
        ReadinessState::NotReady,
        "an empty view that is not authoritative may still hide peers to reclaim from"
    );

    tick(100).await;
    assert_eq!(
        readiness.state(),
        ReadinessState::Ready,
        "past the bootstrap hard timer the node serves with the view it has"
    );
    assert!(!sup.is_running("B"), "no peer is known yet");

    membership.sync(vec![Peer::new("B", "B")]);
    tick(500).await;
    assert!(sup.is_running("B"), "B's flows start once the view lists it");
    assert!(sup.is_current("B"), "and B's Reclaim flow catches up");
    assert_eq!(readiness.state(), ReadinessState::Ready, "readiness stays latched");
}
