//! What a worker's own membership shows: the peers a test tells it of, and
//! its own endpoint, bound to its live incarnation.

use topology::Peer;

use super::ReplicatedB2buaSut;
use crate::views::Belief;

impl ReplicatedB2buaSut {
    /// Drive a `MemberDelta::Removed` for `ordinal` into THIS node's membership —
    /// the simulation of k8s dropping a killed pod's endpoint from the survivor's
    /// view. The node's supervisor reconciles it to a Park, and the node takes
    /// over the departed peer's calls with an INVITE in flight (ADR-0047); a
    /// quiet dialog is taken over only when the proxy reroutes its traffic.
    /// A kill the peers are not told of (ADR-0031 cases 3a, 4) skips this.
    pub fn simulate_peer_removed(&self, ordinal: &str) {
        self.membership.remove(ordinal);
    }

    /// Drive a `MemberDelta::Added` for `ordinal` into THIS node's membership —
    /// the simulation of k8s re-publishing a restarted pod's endpoint. The
    /// survivor's supervisor re-spawns its puller to the peer (seeded from the
    /// retained watermark) → fresh forward replication. A statefulset restart is
    /// observed as Removed-then-Added; pair this with a prior
    /// [`simulate_peer_removed`](Self::simulate_peer_removed). Host == ordinal,
    /// matching the harness's `Peer::new(p, p)` convention.
    pub fn simulate_peer_added(&self, ordinal: &str) {
        self.membership.add(Peer::new(ordinal, ordinal));
    }

    /// List this incarnation's own endpoint under its identity, as the
    /// informer lists a restarted pod once ready, unless the orchestrator
    /// departed or withdrew the ordinal: [`readmit`](crate::FailoverHarness::readmit)
    /// lists it then.
    pub(super) fn list_own_endpoint(&self) {
        let left = matches!(
            self.views.beliefs("orchestrator", &self.ordinal).last(),
            Some(Belief::Departed | Belief::Withdrawn)
        );
        if !left {
            self.membership
                .add(Peer::new(&self.ordinal, &self.ordinal).with_identity(self.incarnation_key()));
        }
    }
}
