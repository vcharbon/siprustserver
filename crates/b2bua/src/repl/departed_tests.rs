//! The departed probe answers `true` only for a peer this node pulled and
//! parked when it left the membership set: a peer never seen, pulled while
//! ready, or kept on presence while not ready has not departed, and a peer
//! that rejoins has not either.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use topology::{Membership, Peer, SimulatedMembership};

use super::test_support::{fast_config, supervisor_for, tick, Node};
use super::{departed_probe, PeerLink, ReplicatingCallStore};

#[tokio::test(start_paused = true)]
async fn only_a_parked_peer_has_departed() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let b = SocketAddr::from(([127, 0, 0, 1], 9451));
    let _b = Node::spawn("B", b, 5, &net, &clock).await;
    let store = ReplicatingCallStore::new(6, clock.clone());
    let sup = supervisor_for("A", &store, &net, &clock, vec![("B".into(), b)], fast_config());
    let membership =
        Arc::new(SimulatedMembership::with_clock(vec![Peer::new("B", "B")], clock.clone()));
    sup.start(membership.clone() as Arc<dyn Membership>);
    tick(1_000).await;
    let departed = departed_probe(&sup);

    assert_eq!(sup.peer_link("Z"), PeerLink::Absent);
    assert!(!departed("Z"), "a peer never seen has not departed");
    assert_eq!(sup.peer_link("B"), PeerLink::Active);
    assert!(!departed("B"), "a pulled ready peer has not departed");

    membership.set_conditions("B", false, true);
    tick(500).await;
    assert_eq!(sup.peer_link("B"), PeerLink::Kept);
    assert!(!departed("B"), "a peer kept on presence has not departed");

    membership.remove("B");
    tick(500).await;
    assert_eq!(sup.peer_link("B"), PeerLink::Parked);
    assert!(departed("B"), "a peer parked on leaving the set has departed");

    membership.add(Peer::new("B", "B"));
    tick(1_000).await;
    assert!(!departed("B"), "a peer that rejoined has not departed");
    sup.shutdown();
}
