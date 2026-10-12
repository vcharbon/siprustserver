//! The name lookup under the [`NamedForwarder`](super::NamedForwarder): the
//! [`HostResolver`] seam and the system resolver behind it.

use std::net::SocketAddr;

use async_trait::async_trait;

/// Resolve a `(host, port)` name to its addresses, in the resolver's order;
/// empty when the name does not resolve. Seam over the system resolver so
/// tests can inject failures and latency and count calls.
#[async_trait]
pub trait HostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Vec<SocketAddr>;
}

/// Candidate query strings for `host`, in attempt order.
///
/// Kubernetes resolv.conf ships `ndots:5`, so a Service FQDN like
/// `svc.ns.svc.cluster.local` (4 dots < 5) is first expanded through every
/// search domain — several serial lookups before the name is tried as-is.
/// Appending the root dot makes the name absolute, which getaddrinfo resolves
/// in one lookup:
/// - dotted and not already absolute → `["{host}.", host]` (the raw fallback
///   keeps short names that need search expansion working);
/// - single-label (only search expansion can resolve it) or already absolute
///   → `[host]`.
pub(crate) fn resolution_candidates(host: &str) -> Vec<String> {
    if host.contains('.') && !host.ends_with('.') {
        vec![format!("{host}."), host.to_string()]
    } else {
        vec![host.to_string()]
    }
}

/// Try each of [`resolution_candidates`] in order through `lookup`; the
/// first candidate that yields any address wins.
pub(crate) async fn resolve_first<F, Fut>(host: &str, port: u16, lookup: F) -> Vec<SocketAddr>
where
    F: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = Vec<SocketAddr>>,
{
    for candidate in resolution_candidates(host) {
        let addrs = lookup(candidate, port).await;
        if !addrs.is_empty() {
            return addrs;
        }
    }
    Vec::new()
}

/// The production resolver — `tokio::net::lookup_host` (getaddrinfo on the
/// blocking pool). Dotted names are queried absolutely-qualified first (see
/// `resolution_candidates`).
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Vec<SocketAddr> {
        resolve_first(host, port, |candidate, port| async move {
            match tokio::net::lookup_host((candidate.as_str(), port)).await {
                Ok(addrs) => addrs.collect(),
                Err(_) => Vec::new(),
            }
        })
        .await
    }
}
