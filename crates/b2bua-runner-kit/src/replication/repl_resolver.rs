//! [`ReplResolver`] — a replication peer's address, looked up afresh on every
//! connect attempt (ADR-0012 D3).
//!
//! A static peer's host is used as given: an IP, or a name looked up. A
//! discovered peer is reached by its stable pod DNS name, falling back to its
//! membership host (the pod IP) when the name does not resolve. `None` makes
//! the puller back off and retry.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::repl::PeerResolver;
use http_net::NameResolver;
use topology::Peer;

use super::{pod_dns_name, ReplAddressing};

/// Resolves a peer to `<address>:repl_port` on every connect attempt.
pub(crate) struct ReplResolver {
    repl_port: u16,
    addressing: ReplAddressing,
    names: Arc<dyn NameResolver>,
}

impl ReplResolver {
    /// A resolver reaching every peer on `repl_port`, looking names up through
    /// `names`.
    pub(crate) fn new(
        repl_port: u16,
        addressing: ReplAddressing,
        names: Arc<dyn NameResolver>,
    ) -> Self {
        Self { repl_port, addressing, names }
    }

    /// The first address `host` resolves to on the replication port.
    async fn look_up(&self, host: &str) -> Option<SocketAddr> {
        let name = format!("{host}:{}", self.repl_port);
        self.names.resolve(&name).await.into_iter().next()
    }

    /// `host` as an IP on the replication port, never looked up.
    fn as_ip(&self, host: &str) -> Option<SocketAddr> {
        host.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, self.repl_port))
    }
}

#[async_trait]
impl PeerResolver for ReplResolver {
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr> {
        let addr = match &self.addressing {
            ReplAddressing::Static => match self.as_ip(&peer.host) {
                Some(addr) => Some(addr),
                None => self.look_up(&peer.host).await,
            },
            ReplAddressing::PodDns { service, namespace } => {
                let name = pod_dns_name(&peer.ordinal, service, namespace);
                match self.look_up(&name).await {
                    Some(addr) => Some(addr),
                    None => self.as_ip(&peer.host),
                }
            }
        };
        // One line per attempt: a new address after a restart shows the
        // puller followed the peer.
        match addr {
            Some(a) => tracing::info!(peer = %peer.ordinal, addr = %a, "repl peer resolved"),
            None => tracing::warn!(peer = %peer.ordinal, "repl peer unresolvable (will retry)"),
        }
        addr
    }
}

#[cfg(test)]
mod tests;
