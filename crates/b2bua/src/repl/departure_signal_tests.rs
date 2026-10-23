//! The supervisor tells the router a peer departed once per real stop: the
//! instant the peer leaves the membership set and its running flows are
//! parked. A periodic reconcile of a peer already parked, a condition flip of
//! a peer still in the set, and this node's own shutdown say nothing.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::mpsc;
use topology::{Membership, Peer, SimulatedMembership};

use super::test_support::{fast_config, supervisor_for, tick, Node};
use super::ReplicatingCallStore;
use crate::router::ReplCommand;

fn addr(n: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9400 + n))
}

/// Every departure the router has been told of so far, in order.
fn departures(rx: &mut mpsc::UnboundedReceiver<ReplCommand>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let ReplCommand::PeerDeparted(ordinal) = cmd {
            seen.push(ordinal);
        }
    }
    seen
}

#[tokio::test(start_paused = true)]
async fn a_peer_leaving_the_membership_set_is_announced_once_per_stop() {
    let clock = Clock::test_at(0);
    let net = Arc::new(SimulatedReplicationNetwork::with_delay(1));
    let (b, c) = (addr(1), addr(2));
    let _b = Node::spawn("B", b, 5, &net, &clock).await;
    let _c = Node::spawn("C", c, 5, &net, &clock).await;

    let store = ReplicatingCallStore::new(6, clock.clone());
    let peers = vec![("B".into(), b), ("C".into(), c)];
    let sup = supervisor_for("A", &store, &net, &clock, peers, fast_config());
    let (tx, mut rx) = mpsc::unbounded_channel();
    sup.set_repl_sink(tx);
    let membership = Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new("B", "B"), Peer::new("C", "C")],
        clock.clone(),
    ));
    sup.start(membership.clone() as Arc<dyn Membership>);
    tick(1_000).await;
    assert!(sup.is_running("B") && sup.is_running("C"), "both peers pulled");
    assert!(departures(&mut rx).is_empty(), "a peer joining is no departure");

    membership.set_conditions("C", false, true);
    tick(500).await;
    assert!(departures(&mut rx).is_empty(), "a peer still in the set has not departed");

    membership.remove("B");
    tick(500).await;
    assert_eq!(departures(&mut rx), ["B"], "the stop is announced once");

    // Several periodic reconciles see B absent again: B is parked already.
    tick(12_000).await;
    assert!(departures(&mut rx).is_empty(), "a reconcile of a parked peer announces nothing");

    membership.add(Peer::new("B", "B"));
    tick(1_000).await;
    assert!(sup.is_running("B"), "B pulled again");
    membership.remove("B");
    tick(500).await;
    assert_eq!(departures(&mut rx), ["B"], "a second real stop is announced again");

    sup.shutdown();
    tick(500).await;
    assert!(departures(&mut rx).is_empty(), "this node's own shutdown announces no peer");
}
