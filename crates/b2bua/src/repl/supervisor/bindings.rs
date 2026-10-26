//! What every reconciled snapshot binds (ADR-0031 D6, ADR-0055 §6): this
//! node's own endpoint, with its withdrawal from routing and its supersession,
//! and the worker identity of each peer ordinal its changelog counts flow
//! positions from.

use tokio::sync::watch;
use topology::Peer;

use super::super::self_endpoint::SelfEndpoint;
use super::ReplicationSupervisor;

impl ReplicationSupervisor {
    /// Read this node's own endpoint and every peer's identity out of
    /// `snapshot`.
    pub(super) fn observe_bindings(&self, snapshot: &[Peer]) {
        self.observe_self(snapshot);
        let changelog = self.inner.store.changelog();
        for peer in snapshot.iter().filter(|p| p.ordinal != self.inner.self_ordinal) {
            changelog.bind_identity(&peer.ordinal, peer.identity.as_deref());
        }
    }

    /// Read this node's own endpoint out of `snapshot` (ADR-0031 D6). A
    /// withdrawal raises the `b2bua_withdrawn_running` gauge; the readiness
    /// latch follows through [`self_endpoint`](Self::self_endpoint).
    fn observe_self(&self, snapshot: &[Peer]) {
        let synced = self.membership_synced();
        if let Some(SelfEndpoint::Withdrawn(_)) = self.inner.self_endpoint.observe(snapshot, synced)
        {
            self.inner.metrics.set_withdrawn_running(true);
        }
    }

    /// Whether this node has observed its own endpoint withdrawn from routing
    /// (ADR-0031 D6): nothing new is routed here. Sticky. Always `false` on a
    /// membership that does not show the node itself (static list).
    pub fn is_withdrawn(&self) -> bool {
        self.inner.self_endpoint.is_withdrawn()
    }

    /// The node's own endpoint state, as a watch that wakes on each transition.
    pub fn self_endpoint(&self) -> watch::Receiver<SelfEndpoint> {
        self.inner.self_endpoint.subscribe()
    }

    /// Whether the latest snapshot shows this node's endpoint ready, as a
    /// watch; `None` when the membership does not show the node itself (set
    /// by `start`).
    pub fn own_endpoint_ready(&self) -> Option<watch::Receiver<bool>> {
        self.inner.self_endpoint.ready_now()
    }
}
