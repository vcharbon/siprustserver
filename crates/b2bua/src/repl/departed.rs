//! Whether a peer has departed: this node pulled it, it left the membership
//! set and has not rejoined (ADR-0047). The router reads it of a call's
//! primary before it leaves the call's discharge to that primary.

use std::sync::Arc;

use super::{PeerLink, ReplicationSupervisor};

/// Whether the peer of this ordinal has departed.
pub type DepartedProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The probe over `supervisor`'s links: a parked peer has departed; a peer
/// pulled, or never seen, has not.
pub fn departed_probe(supervisor: &ReplicationSupervisor) -> DepartedProbe {
    let supervisor = supervisor.clone();
    Arc::new(move |peer: &str| supervisor.peer_link(peer) == PeerLink::Parked)
}
