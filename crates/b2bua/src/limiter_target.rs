//! [`LimiterTarget`] — where the limiter client sends its requests.
//!
//! A target is a socket address, or a `host:port` name resolved on the
//! request path by a [`NameResolver`]: a name that does not resolve fails the
//! request like a transport error, inside the request's budget, so a limiter
//! whose name resolves late is reached once it does. One lookup is in flight
//! at a time; a request arriving meanwhile fails at once instead of starting
//! another. The first address a name resolves to is kept for the life of the
//! client.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

/// Resolves a `host:port` name to one socket address.
#[async_trait]
pub trait NameResolver: Send + Sync {
    /// The first address `name` resolves to, `None` when it does not.
    async fn resolve(&self, name: &str) -> Option<SocketAddr>;
}

/// The host's resolver (`getaddrinfo` on the blocking pool).
pub struct SystemResolver;

#[async_trait]
impl NameResolver for SystemResolver {
    async fn resolve(&self, name: &str) -> Option<SocketAddr> {
        tokio::net::lookup_host(name).await.ok()?.next()
    }
}

/// The limiter's address, or the name that resolves to it.
pub struct LimiterTarget {
    kind: Kind,
}

enum Kind {
    Addr(SocketAddr),
    Name(Arc<Named>),
}

struct Named {
    name: String,
    resolver: Arc<dyn NameResolver>,
    resolved: OnceLock<SocketAddr>,
    /// A lookup is in flight.
    looking_up: AtomicBool,
}

/// Clears `looking_up` when the lookup ends, a panicking one included.
struct LookupDone(Arc<Named>);

impl Drop for LookupDone {
    fn drop(&mut self) {
        self.0.looking_up.store(false, Ordering::Release);
    }
}

impl LimiterTarget {
    /// A target at `addr`.
    pub fn addr(addr: SocketAddr) -> Self {
        Self { kind: Kind::Addr(addr) }
    }

    /// A target named `host:port`, resolved by the host's resolver.
    pub fn name(name: impl Into<String>) -> Self {
        Self::name_with(name, Arc::new(SystemResolver))
    }

    /// A target named `host:port`, resolved by `resolver` on the first
    /// request that needs it and on every request until it resolves.
    pub fn name_with(name: impl Into<String>, resolver: Arc<dyn NameResolver>) -> Self {
        Self {
            kind: Kind::Name(Arc::new(Named {
                name: name.into(),
                resolver,
                resolved: OnceLock::new(),
                looking_up: AtomicBool::new(false),
            })),
        }
    }

    /// The address to send to; `None` while the name does not resolve, or
    /// while another request's lookup is in flight. The lookup runs in its
    /// own task: a caller that gives up waiting leaves it to finish.
    pub async fn resolve(&self) -> Option<SocketAddr> {
        let named = match &self.kind {
            Kind::Addr(addr) => return Some(*addr),
            Kind::Name(named) => named,
        };
        if let Some(addr) = named.resolved.get() {
            return Some(*addr);
        }
        if named.looking_up.swap(true, Ordering::AcqRel) {
            return None;
        }
        let done = LookupDone(named.clone());
        let lookup = tokio::spawn(async move {
            let named = done.0.clone();
            let addr = named.resolver.resolve(&named.name).await?;
            let addr = *named.resolved.get_or_init(|| addr);
            drop(done);
            Some(addr)
        });
        lookup.await.ok().flatten()
    }
}

impl std::fmt::Display for LimiterTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Kind::Addr(addr) => addr.fmt(f),
            Kind::Name(named) => f.write_str(&named.name),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Resolves the names it knows, after the test lets a lookup go when
    /// `gated`; counts every lookup.
    #[derive(Default)]
    pub(crate) struct FakeResolver {
        pub(crate) names: std::sync::Mutex<HashMap<String, SocketAddr>>,
        pub(crate) lookups: AtomicUsize,
        gate: Option<tokio::sync::Notify>,
    }

    #[async_trait]
    impl NameResolver for FakeResolver {
        async fn resolve(&self, name: &str) -> Option<SocketAddr> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            self.names.lock().unwrap().get(name).copied()
        }
    }

    fn addr() -> SocketAddr {
        "10.0.0.1:8080".parse().unwrap()
    }

    #[tokio::test]
    async fn an_address_is_its_own_resolution() {
        assert_eq!(LimiterTarget::addr(addr()).resolve().await, Some(addr()));
    }

    #[tokio::test]
    async fn a_name_resolves_once_it_is_known_and_keeps_its_address() {
        let resolver = Arc::new(FakeResolver::default());
        let target = LimiterTarget::name_with("limiter:8080", resolver.clone());
        assert_eq!(target.resolve().await, None, "not known yet");
        resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
        assert_eq!(target.resolve().await, Some(addr()));
        resolver.names.lock().unwrap().clear();
        assert_eq!(target.resolve().await, Some(addr()), "the resolved address is kept");
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 2);
        assert_eq!(target.to_string(), "limiter:8080");
    }

    #[tokio::test]
    async fn one_lookup_is_in_flight_and_a_request_meanwhile_fails_at_once() {
        let resolver =
            Arc::new(FakeResolver { gate: Some(tokio::sync::Notify::new()), ..Default::default() });
        resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
        let target = Arc::new(LimiterTarget::name_with("limiter:8080", resolver.clone()));
        let first = tokio::spawn({
            let target = target.clone();
            async move { target.resolve().await }
        });
        sip_clock::testkit::settle().await;
        assert_eq!(target.resolve().await, None, "a lookup is in flight");
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
        resolver.gate.as_ref().unwrap().notify_one();
        assert_eq!(first.await.unwrap(), Some(addr()));
        assert_eq!(target.resolve().await, Some(addr()));
    }

    #[tokio::test]
    async fn a_lookup_its_caller_gave_up_on_still_lands() {
        let resolver =
            Arc::new(FakeResolver { gate: Some(tokio::sync::Notify::new()), ..Default::default() });
        resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
        let target = LimiterTarget::name_with("limiter:8080", resolver.clone());
        let gave_up =
            tokio::time::timeout(std::time::Duration::from_millis(10), target.resolve()).await;
        assert!(gave_up.is_err(), "the caller's budget ran out");
        resolver.gate.as_ref().unwrap().notify_one();
        sip_clock::testkit::settle().await;
        assert_eq!(target.resolve().await, Some(addr()));
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
    }
}
