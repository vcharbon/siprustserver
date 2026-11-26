//! The name lookup under the [`NamedForwarder`](super::NamedForwarder): the
//! [`HostResolver`] seam and the system resolver behind it.

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::time::Instant;

/// Resolve a `(host, port)` name to its addresses, in the resolver's order;
/// empty when the name does not resolve. No attempt starts at or past
/// `deadline`, so a lookup that outlives it ends with the attempt already
/// running. Seam over the system resolver so tests can inject failures and
/// latency and count calls.
#[async_trait]
pub trait HostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16, deadline: Instant) -> Vec<SocketAddr>;
}

/// Candidate query strings for `host`, in attempt order: a dotted name not
/// already absolute is tried with the root dot first, so the resolver's
/// search list is not walked before the name itself, then as given; a
/// single-label or absolute name is tried once, as given.
pub(crate) fn resolution_candidates(host: &str) -> Vec<String> {
    if host.contains('.') && !host.ends_with('.') {
        vec![format!("{host}."), host.to_string()]
    } else {
        vec![host.to_string()]
    }
}

/// Try each of [`resolution_candidates`] in order through `lookup`; the
/// first candidate that yields any address wins. A candidate after the first
/// starts only before `deadline`.
pub(crate) async fn resolve_first<F, Fut>(
    host: &str,
    port: u16,
    deadline: Instant,
    lookup: F,
) -> Vec<SocketAddr>
where
    F: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = Vec<SocketAddr>>,
{
    for (attempt, candidate) in resolution_candidates(host).into_iter().enumerate() {
        if attempt > 0 && Instant::now() >= deadline {
            break;
        }
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
    async fn resolve(&self, host: &str, port: u16, deadline: Instant) -> Vec<SocketAddr> {
        resolve_first(host, port, deadline, |candidate, port| async move {
            match tokio::net::lookup_host((candidate.as_str(), port)).await {
                Ok(addrs) => addrs.collect(),
                Err(_) => Vec::new(),
            }
        })
        .await
    }
}
