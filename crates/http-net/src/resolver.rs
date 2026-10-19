//! [`NameResolver`] — how a `host:port` name becomes socket addresses: the
//! host's resolver in production, [`MemoryResolver`] in tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::poison::{locked, MEMORY_RESOLVER};

/// Resolves a `host:port` name to socket addresses.
#[async_trait]
pub trait NameResolver: Send + Sync {
    /// Every address `name` resolves to, in the resolver's order; empty when
    /// it does not resolve.
    async fn resolve(&self, name: &str) -> Vec<SocketAddr>;
}

/// The host's resolver (`getaddrinfo` on the blocking pool).
pub struct SystemResolver;

#[async_trait]
impl NameResolver for SystemResolver {
    async fn resolve(&self, name: &str) -> Vec<SocketAddr> {
        match tokio::net::lookup_host(name).await {
            Ok(addrs) => addrs.collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// A resolver over names a test points: each lookup takes the delay set,
/// then answers what the name points to now. Counts every lookup started.
#[derive(Default)]
pub struct MemoryResolver {
    names: Mutex<HashMap<String, Vec<SocketAddr>>>,
    delay: Mutex<Duration>,
    lookups: AtomicUsize,
}

impl MemoryResolver {
    /// A resolver that knows no name.
    pub fn new() -> Self {
        Self::default()
    }

    /// `name` resolves to `addrs` from now on.
    pub fn point(&self, name: &str, addrs: impl IntoIterator<Item = SocketAddr>) {
        let addrs = addrs.into_iter().collect();
        locked(&self.names, MEMORY_RESOLVER).insert(name.to_string(), addrs);
    }

    /// `name` resolves to nothing from now on.
    pub fn unpoint(&self, name: &str) {
        locked(&self.names, MEMORY_RESOLVER).remove(name);
    }

    /// Every lookup started from now on takes `delay` before it answers.
    pub fn slow(&self, delay: Duration) {
        *locked(&self.delay, MEMORY_RESOLVER) = delay;
    }

    /// The lookups started so far.
    pub fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl NameResolver for MemoryResolver {
    async fn resolve(&self, name: &str) -> Vec<SocketAddr> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let delay = *locked(&self.delay, MEMORY_RESOLVER);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        locked(&self.names, MEMORY_RESOLVER).get(name).cloned().unwrap_or_default()
    }
}
