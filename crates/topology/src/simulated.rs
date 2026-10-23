//! [`SimulatedMembership`] — a membership a test drives by hand.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sip_clock::Clock;
use tokio::sync::broadcast;

use crate::{MemberDelta, Membership, MembershipState, Peer};

/// A membership whose peer set is driven imperatively by a test. Cheap to clone
/// — clones share the same underlying state + delta channel. Clock-injected
/// (mirrors the proxy's `SimulatedWorkerRegistry`); membership mutations
/// themselves carry no behavioural time.
#[derive(Clone)]
pub struct SimulatedMembership {
    state: Arc<MembershipState>,
    /// Whether the view is authoritative ([`Membership::synced`]).
    synced: Arc<AtomicBool>,
    #[allow(dead_code)]
    clock: Clock,
}

impl SimulatedMembership {
    /// Build with the system clock.
    pub fn new(initial: Vec<Peer>) -> Self {
        Self::with_clock(initial, Clock::system())
    }

    /// Build with an injected clock (tests use `Clock::test_at(..)`).
    pub fn with_clock(initial: Vec<Peer>, clock: Clock) -> Self {
        Self {
            state: Arc::new(MembershipState::new(initial)),
            synced: Arc::new(AtomicBool::new(true)),
            clock,
        }
    }

    /// An informer that has not completed its first listing: an empty view,
    /// not authoritative until [`sync`](Self::sync).
    pub fn unsynced(clock: Clock) -> Self {
        let membership = Self::with_clock(Vec::new(), clock);
        membership.synced.store(false, Ordering::SeqCst);
        membership
    }

    /// The first listing completes: the view holds `peers` and is
    /// authoritative from now on. Each peer is an [`Added`](MemberDelta::Added).
    pub fn sync(&self, peers: Vec<Peer>) {
        for peer in peers {
            self.state.add(peer);
        }
        self.synced.store(true, Ordering::SeqCst);
    }

    /// Add (or replace) a peer, emitting [`MemberDelta::Added`].
    pub fn add(&self, peer: Peer) {
        self.state.add(peer);
    }

    /// Remove the peer with `ordinal`, emitting [`MemberDelta::Removed`] (no-op
    /// if absent).
    pub fn remove(&self, ordinal: &str) {
        self.state.remove(ordinal);
    }

    /// Change an existing peer's binding — host or worker identity — emitting
    /// [`MemberDelta::AddressChanged`] (no-op if unknown or unchanged).
    pub fn change_address(&self, peer: Peer) {
        self.state.set_address(peer);
    }

    /// Set an existing peer's endpoint conditions, emitting
    /// [`MemberDelta::ConditionsChanged`] (no-op if unknown or unchanged). The
    /// peer stays in the snapshot: it is still pulled, and routable iff `ready`
    /// (ADR-0031 D1).
    pub fn set_conditions(&self, ordinal: &str, ready: bool, terminating: bool) {
        let Some(cur) = self.state.resolve(ordinal) else {
            return;
        };
        self.state.set_conditions(Peer { ready, terminating, ..cur });
    }

    /// Resolve a peer by ordinal (test introspection).
    pub fn resolve(&self, ordinal: &str) -> Option<Peer> {
        self.state.resolve(ordinal)
    }
}

impl Membership for SimulatedMembership {
    fn snapshot(&self) -> Vec<Peer> {
        self.state.snapshot()
    }
    fn changes(&self) -> broadcast::Receiver<MemberDelta> {
        self.state.changes()
    }
    fn synced(&self) -> bool {
        self.synced.load(Ordering::SeqCst)
    }
    /// The simulation stands in for the informer: a node's own ordinal is in
    /// its snapshot while the orchestrator publishes it.
    fn observes_self(&self) -> bool {
        true
    }
}
