//! [`ReplResolver`] — a replication peer's address, looked up afresh on every
//! connect attempt (ADR-0012 D3).
//!
//! A static peer's host is used as given: an IP, or a name looked up. A
//! discovered peer is reached by its stable pod DNS name, falling back to its
//! membership host (the pod IP) when the name does not resolve. `None` makes
//! the puller back off and retry.
//!
//! Every lookup is bounded (ADR-0045) and single-flight per name: a lookup past
//! its bound answers nothing and runs on until it ends by itself, and while a
//! few of one name's lookups are stuck so, no new one starts
//! ([`ResolvedTarget`], ADR-0043).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::repl::PeerResolver;
use http_net::{LookupSettings, NameResolver, ResolvedTarget};
use topology::Peer;

use super::{pod_dns_name, ReplAddressing};
use crate::poison::{locked, REPL_TARGETS};

/// Resolves a peer to `<address>:repl_port` on every connect attempt.
pub(crate) struct ReplResolver {
    repl_port: u16,
    addressing: ReplAddressing,
    names: Arc<dyn NameResolver>,
    lookup: LookupSettings,
    /// One target per name looked up, so its lookups are single-flight.
    targets: Mutex<HashMap<String, Arc<ResolvedTarget>>>,
}

impl ReplResolver {
    /// A resolver reaching every peer on `repl_port`, looking names up through
    /// `names`, each lookup bounded by `lookup_timeout`.
    pub(crate) fn new(
        repl_port: u16,
        addressing: ReplAddressing,
        names: Arc<dyn NameResolver>,
        lookup_timeout: Duration,
    ) -> Self {
        let lookup = LookupSettings { bound: lookup_timeout, ..LookupSettings::default() };
        Self { repl_port, addressing, names, lookup, targets: Mutex::default() }
    }

    /// The first address `host` resolves to on the replication port, looked
    /// up afresh.
    async fn look_up(&self, host: &str) -> Option<SocketAddr> {
        let name = format!("{host}:{}", self.repl_port);
        let target = locked(&self.targets, REPL_TARGETS)
            .entry(name.clone())
            .or_insert_with(|| {
                Arc::new(ResolvedTarget::name_with_lookup(name, self.names.clone(), self.lookup))
            })
            .clone();
        target.forget();
        target.resolve().await
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
