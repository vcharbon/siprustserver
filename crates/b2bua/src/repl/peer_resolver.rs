//! [`PeerResolver`] — how a supervisor's pullers find a peer's replication
//! address, on every connect attempt (ADR-0012 D3).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use topology::Peer;

/// Resolves a [`Peer`] to its replication [`SocketAddr`], **fresh on every
/// connect attempt** (ADR-0012 D3). Async so a real impl can DNS-resolve a stable
/// per-pod name (`tokio::net::lookup_host`) without blocking the runtime; `None`
/// means "unresolvable right now" — the puller treats it as a failed connect and
/// backs off, then retries (re-resolving). For sim tests / bare-IP hosts this is
/// an instant map lookup or parse.
#[async_trait]
pub trait PeerResolver: Send + Sync {
    /// Resolve `peer` to its replication socket address, or `None` if it cannot
    /// be resolved at this instant.
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr>;
}

/// Shared handle to a [`PeerResolver`].
pub type AddrResolver = Arc<dyn PeerResolver>;

/// Adapter turning a sync closure (`Fn(&Peer) -> SocketAddr`) into a
/// [`PeerResolver`] — the sim-test / static-map resolver shape. Always resolves
/// (never `None`).
pub struct FnPeerResolver<F>(pub F);

#[async_trait]
impl<F> PeerResolver for FnPeerResolver<F>
where
    F: Fn(&Peer) -> SocketAddr + Send + Sync,
{
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr> {
        Some((self.0)(peer))
    }
}
