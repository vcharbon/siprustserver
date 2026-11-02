//! [`PeerLink`] — this node's link state toward one peer, as the supervisor
//! holds it: read-only introspection.

use super::ReplicationSupervisor;

/// This node's link state toward one peer, as the supervisor holds it —
/// read-only introspection ([`peer_link`](ReplicationSupervisor::peer_link)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLink {
    /// The ordinal was never in this node's membership view.
    Absent,
    /// The peer left the membership set: both flows are interrupted, the
    /// retained watermarks survive for a warm resume.
    Parked,
    /// The Reclaim flow's puller is running toward a ready peer.
    Active,
    /// The Reclaim flow's puller is running toward a member that is present in
    /// the snapshot but not `ready` — pulled on presence alone (ADR-0031 D1).
    Kept,
}

impl ReplicationSupervisor {
    /// This node's link state toward `peer` ([`PeerLink`]): `Active` while its
    /// Reclaim puller runs toward a ready member, `Kept` while it runs toward a
    /// member that is present but not ready, `Parked` once it left the
    /// membership set (the entry and its watermarks are retained), `Absent` when
    /// the ordinal was never seen. Pure read — it folds no puller status and
    /// spawns nothing.
    pub fn peer_link(&self, peer: &str) -> PeerLink {
        match self.inner.peers().get(peer) {
            None => PeerLink::Absent,
            Some(e) if e.reclaim.status_rx.is_some() && e.ready => PeerLink::Active,
            Some(e) if e.reclaim.status_rx.is_some() => PeerLink::Kept,
            Some(_) => PeerLink::Parked,
        }
    }
}
