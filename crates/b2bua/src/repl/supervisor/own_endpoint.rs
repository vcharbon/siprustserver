//! The node's own endpoint as every reconciled snapshot shows it (ADR-0031
//! D6, ADR-0055 §6): its withdrawal from routing and its supersession.

use tokio::sync::watch;
use topology::Peer;

use super::super::self_endpoint::SelfEndpoint;
use super::ReplicationSupervisor;

impl ReplicationSupervisor {
    /// Read this node's own endpoint out of `snapshot` (ADR-0031 D6). A
    /// withdrawal raises the `b2bua_withdrawn_running` gauge; the readiness
    /// latch follows through [`self_endpoint`](Self::self_endpoint).
    pub(super) fn observe_self(&self, snapshot: &[Peer]) {
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

    /// [`self_endpoint`](Self::self_endpoint) when the membership shows the
    /// node itself (set by `start`); `None` on one that never will.
    pub fn observed_self_endpoint(&self) -> Option<watch::Receiver<SelfEndpoint>> {
        self.inner.self_endpoint.is_enabled().then(|| self.self_endpoint())
    }
}
