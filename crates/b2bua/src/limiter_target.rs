//! [`LimiterTarget`] — where the limiter client sends its requests.
//!
//! A target is a socket address, or a `host:port` name resolved on the
//! request path by a [`NameResolver`]: a name that does not resolve fails the
//! request like a transport error, inside the request's budget, so a limiter
//! whose name resolves late is reached once it does. A name that parses as a
//! socket address (`10.0.0.1:8080`, `[::1]:8080`) is one, never looked up.
//! One lookup is in flight at a time, and every request arriving meanwhile
//! waits for it within its own budget. The address a name resolves to is
//! kept until [`LimiterTarget::forget`]; the next request then looks the name
//! up again, and a lookup started before the forget is not kept.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use tokio::sync::watch;

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

/// A lookup's answer as its waiters see it: `None` while it runs.
type Answer = Option<Option<SocketAddr>>;

struct Named {
    name: String,
    resolver: Arc<dyn NameResolver>,
    state: Mutex<NameState>,
}

#[derive(Default)]
struct NameState {
    /// The address kept.
    addr: Option<SocketAddr>,
    /// The lookup in flight, if any, and its generation.
    lookup: Option<(u64, watch::Receiver<Answer>)>,
    /// Bumped by every forget: a lookup of an older generation lands for
    /// its waiters only.
    generation: u64,
}

impl Named {
    /// The state. Every step leaves it whole, so a poisoned lock is taken
    /// as it is.
    fn lock(&self) -> MutexGuard<'_, NameState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The address kept, or the lookup in flight, started when none is.
    fn address_or_lookup(self: &Arc<Self>) -> Result<SocketAddr, watch::Receiver<Answer>> {
        let mut state = self.lock();
        if let Some(addr) = state.addr {
            return Ok(addr);
        }
        if let Some((_, waiting)) = state.lookup.as_ref() {
            return Err(waiting.clone());
        }
        let generation = state.generation;
        let (tx, rx) = watch::channel(None);
        state.lookup = Some((generation, rx.clone()));
        drop(state);
        let done = LookupDone { named: self.clone(), generation };
        tokio::spawn(async move {
            let found = done.named.resolver.resolve(&done.named.name).await;
            done.land(found);
            drop(done);
            let _ = tx.send(Some(found));
        });
        Err(rx)
    }
}

/// The lookup of one generation. Landing keeps its address when no forget
/// came since it started; ending, a panicking one included, frees the slot so
/// the next request starts another.
struct LookupDone {
    named: Arc<Named>,
    generation: u64,
}

impl LookupDone {
    fn land(&self, found: Option<SocketAddr>) {
        let mut state = self.named.lock();
        if state.generation == self.generation && state.addr.is_none() {
            state.addr = found;
        }
    }
}

impl Drop for LookupDone {
    fn drop(&mut self) {
        let mut state = self.named.lock();
        if state.lookup.as_ref().is_some_and(|(g, _)| *g == self.generation) {
            state.lookup = None;
        }
    }
}

impl LimiterTarget {
    /// A target at `addr`.
    pub fn addr(addr: SocketAddr) -> Self {
        Self { kind: Kind::Addr(addr) }
    }

    /// A target named `host:port`, resolved by `resolver` on the first
    /// request that needs it and on every request until it resolves; a
    /// socket address is taken as it is.
    pub fn name_with(name: impl Into<String>, resolver: Arc<dyn NameResolver>) -> Self {
        let name = name.into();
        if let Ok(addr) = name.parse::<SocketAddr>() {
            return Self::addr(addr);
        }
        Self {
            kind: Kind::Name(Arc::new(Named {
                name,
                resolver,
                state: Mutex::new(NameState::default()),
            })),
        }
    }

    /// The address to send to; `None` when the name does not resolve. Waits
    /// for the lookup in flight, or starts one; the caller bounds the wait,
    /// and a caller that gives up leaves the lookup to finish for the next.
    pub async fn resolve(&self) -> Option<SocketAddr> {
        let named = match &self.kind {
            Kind::Addr(addr) => return Some(*addr),
            Kind::Name(named) => named,
        };
        let mut lookup = match named.address_or_lookup() {
            Ok(addr) => return Some(addr),
            Err(lookup) => lookup,
        };
        let answer = lookup.wait_for(Option::is_some).await.ok()?;
        answer.flatten()
    }

    /// The address kept now, without a lookup.
    pub fn address(&self) -> Option<SocketAddr> {
        match &self.kind {
            Kind::Addr(addr) => Some(*addr),
            Kind::Name(named) => named.lock().addr,
        }
    }

    /// Forget the address a name resolved to and leave any lookup in flight
    /// to its waiters: the next request looks the name up again. A socket
    /// address is kept.
    pub fn forget(&self) {
        if let Kind::Name(named) = &self.kind {
            let mut state = named.lock();
            state.addr = None;
            state.lookup = None;
            state.generation = state.generation.wrapping_add(1);
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Resolves the names it knows, after the test lets a lookup go when
    /// `gated`; counts every lookup.
    #[derive(Default)]
    pub(crate) struct FakeResolver {
        pub(crate) names: std::sync::Mutex<HashMap<String, SocketAddr>>,
        pub(crate) lookups: AtomicUsize,
        pub(crate) gate: Option<tokio::sync::Notify>,
        /// How long a lookup takes.
        pub(crate) delay: Option<std::time::Duration>,
    }

    #[async_trait]
    impl NameResolver for FakeResolver {
        async fn resolve(&self, name: &str) -> Option<SocketAddr> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
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
    async fn a_forgotten_address_is_looked_up_again() {
        let resolver = Arc::new(FakeResolver::default());
        resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
        let target = LimiterTarget::name_with("limiter:8080", resolver.clone());
        assert_eq!(target.resolve().await, Some(addr()));
        let moved: SocketAddr = "10.0.0.2:8080".parse().unwrap();
        resolver.names.lock().unwrap().insert("limiter:8080".into(), moved);
        assert_eq!(target.resolve().await, Some(addr()), "kept until forgotten");
        target.forget();
        assert_eq!(target.address(), None, "forgotten");
        assert_eq!(target.resolve().await, Some(moved), "looked up again");
        assert_eq!(target.address(), Some(moved));
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_socket_address_is_never_forgotten() {
        let target = LimiterTarget::addr(addr());
        target.forget();
        assert_eq!(target.address(), Some(addr()));
    }

    /// Answers each lookup from a script of `(delay, answer)`.
    #[derive(Default)]
    struct ScriptedResolver {
        script: std::sync::Mutex<std::collections::VecDeque<(u64, Option<SocketAddr>)>>,
        started: AtomicUsize,
    }

    #[async_trait]
    impl NameResolver for ScriptedResolver {
        async fn resolve(&self, _: &str) -> Option<SocketAddr> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let (delay, answer) = self.script.lock().unwrap().pop_front().unwrap_or((0, None));
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            answer
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_in_flight_at_a_forget_lands_and_is_kept() {
        let resolver = Arc::new(ScriptedResolver::default());
        *resolver.script.lock().unwrap() = [(100, Some(addr()))].into();
        let target = Arc::new(LimiterTarget::name_with("limiter:8080", resolver.clone()));
        let first = tokio::spawn({
            let target = target.clone();
            async move { target.resolve().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        target.forget();
        assert_eq!(target.resolve().await, Some(addr()), "waits on the lookup in flight");
        assert_eq!(first.await.unwrap(), Some(addr()));
        assert_eq!(target.address(), Some(addr()), "kept");
        assert!(resolver.script.lock().unwrap().is_empty());
        assert_eq!(resolver.started.load(Ordering::SeqCst), 1, "one lookup in flight at a time");
    }

    #[tokio::test]
    async fn a_socket_address_is_never_looked_up() {
        let resolver = Arc::new(FakeResolver::default());
        for literal in ["10.0.0.1:8080", "[::1]:8080"] {
            let target = LimiterTarget::name_with(literal, resolver.clone());
            assert_eq!(target.resolve().await, Some(literal.parse().unwrap()), "{literal}");
            assert_eq!(target.to_string(), literal.parse::<SocketAddr>().unwrap().to_string());
        }
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_request_during_a_lookup_waits_for_it() {
        let resolver =
            Arc::new(FakeResolver { gate: Some(tokio::sync::Notify::new()), ..Default::default() });
        resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
        let target = Arc::new(LimiterTarget::name_with("limiter:8080", resolver.clone()));
        let first = tokio::spawn({
            let target = target.clone();
            async move { target.resolve().await }
        });
        sip_clock::testkit::settle().await;
        let second = tokio::spawn({
            let target = target.clone();
            async move { target.resolve().await }
        });
        sip_clock::testkit::settle().await;
        assert!(!second.is_finished(), "the second request waits on the lookup");
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1, "one lookup in flight");
        resolver.gate.as_ref().unwrap().notify_one();
        assert_eq!(first.await.unwrap(), Some(addr()));
        assert_eq!(second.await.unwrap(), Some(addr()));
        assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
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
