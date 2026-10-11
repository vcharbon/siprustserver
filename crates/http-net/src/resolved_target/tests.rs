use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;

/// Resolves the names it knows, after the test lets a lookup go when
/// `gated`; counts every lookup.
#[derive(Default)]
struct FakeResolver {
    names: std::sync::Mutex<HashMap<String, SocketAddr>>,
    lookups: AtomicUsize,
    gate: Option<tokio::sync::Notify>,
    /// How long a lookup takes.
    delay: Option<std::time::Duration>,
}

#[async_trait]
impl NameResolver for FakeResolver {
    async fn resolve(&self, name: &str) -> Vec<SocketAddr> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.names.lock().unwrap().get(name).copied().into_iter().collect()
    }
}

fn addr() -> SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

#[tokio::test]
async fn an_address_is_its_own_resolution() {
    assert_eq!(ResolvedTarget::addr(addr()).resolve().await, Some(addr()));
}

#[tokio::test]
async fn a_name_resolves_once_it_is_known_and_keeps_its_address() {
    let resolver = Arc::new(FakeResolver::default());
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
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
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
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
    let target = ResolvedTarget::addr(addr());
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
    async fn resolve(&self, _: &str) -> Vec<SocketAddr> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let (delay, answer) = self.script.lock().unwrap().pop_front().unwrap_or((0, None));
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        answer.into_iter().collect()
    }
}

#[tokio::test(start_paused = true)]
async fn a_lookup_in_flight_at_a_forget_lands_and_is_kept() {
    let resolver = Arc::new(ScriptedResolver::default());
    *resolver.script.lock().unwrap() = [(100, Some(addr()))].into();
    let target = Arc::new(ResolvedTarget::name_with("limiter:8080", resolver.clone()));
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
        let target = ResolvedTarget::name_with(literal, resolver.clone());
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
    let target = Arc::new(ResolvedTarget::name_with("limiter:8080", resolver.clone()));
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

/// Never answers its first `stuck` lookups; answers [`addr`] after that.
/// Counts the lookups started and the ones still running.
#[derive(Default)]
struct StuckResolver {
    stuck: usize,
    started: AtomicUsize,
    running: Arc<AtomicUsize>,
}

/// Decrements the running count when a lookup's future ends or is dropped.
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl NameResolver for StuckResolver {
    async fn resolve(&self, _: &str) -> Vec<SocketAddr> {
        let n = self.started.fetch_add(1, Ordering::SeqCst);
        self.running.fetch_add(1, Ordering::SeqCst);
        let _running = Running(self.running.clone());
        if n < self.stuck {
            std::future::pending::<()>().await;
        }
        vec![addr()]
    }
}

/// A lookup is bounded at 2 s: past it the request fails, the slot is
/// freed, and the next request starts a new lookup that resolves.
#[tokio::test(start_paused = true)]
async fn a_lookup_that_never_returns_does_not_block_a_later_one() {
    let resolver = Arc::new(StuckResolver { stuck: 1, ..Default::default() });
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    let first =
        tokio::time::timeout(std::time::Duration::from_millis(2_100), target.resolve()).await;
    assert_eq!(first, Ok(None), "the stuck lookup fails its request within 2 s");
    let second =
        tokio::time::timeout(std::time::Duration::from_millis(100), target.resolve()).await;
    assert_eq!(second, Ok(Some(addr())), "a later request starts a lookup of its own");
    assert_eq!(resolver.started.load(Ordering::SeqCst), 2);
}

/// A lookup past its bound is left to finish on its own. While 4 of them
/// still run, a request starts no new lookup and fails at once.
#[tokio::test(start_paused = true)]
async fn no_lookup_starts_while_four_are_stuck() {
    let resolver = Arc::new(StuckResolver { stuck: usize::MAX, ..Default::default() });
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    for n in 0..4 {
        let answer =
            tokio::time::timeout(std::time::Duration::from_millis(2_100), target.resolve()).await;
        assert_eq!(answer, Ok(None), "lookup {n} fails its request within 2 s");
    }
    assert_eq!(resolver.running.load(Ordering::SeqCst), 4, "the 4 stuck lookups still run");
    let start = tokio::time::Instant::now();
    let fifth =
        tokio::time::timeout(std::time::Duration::from_millis(2_100), target.resolve()).await;
    assert_eq!(fifth, Ok(None), "the request fails");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(10),
        "it fails at once: {:?}",
        start.elapsed()
    );
    assert_eq!(resolver.started.load(Ordering::SeqCst), 4, "no fifth lookup started");
}

#[tokio::test]
async fn a_lookup_its_caller_gave_up_on_still_lands() {
    let resolver =
        Arc::new(FakeResolver { gate: Some(tokio::sync::Notify::new()), ..Default::default() });
    resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    let gave_up =
        tokio::time::timeout(std::time::Duration::from_millis(10), target.resolve()).await;
    assert!(gave_up.is_err(), "the caller's budget ran out");
    resolver.gate.as_ref().unwrap().notify_one();
    sip_clock::testkit::settle().await;
    assert_eq!(target.resolve().await, Some(addr()));
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
}

/// Every address a name resolves to is returned, in the resolver's order.
#[tokio::test]
async fn every_address_found_is_returned() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    let second: SocketAddr = "10.0.0.2:8080".parse().unwrap();
    resolver.point("limiter:8080", [addr(), second]);
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve_all().await.as_deref(), Some(&[addr(), second][..]));
    assert_eq!(target.address(), Some(addr()));
}

/// A stuck lookup that ends at last stops counting: a lookup starts again.
#[tokio::test(start_paused = true)]
async fn a_stuck_lookup_that_ends_frees_its_count() {
    let resolver = Arc::new(ScriptedResolver::default());
    *resolver.script.lock().unwrap() =
        [(10_000, None), (10_000, None), (10_000, None), (10_000, None), (0, Some(addr()))].into();
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    for _ in 0..MAX_STUCK {
        assert_eq!(target.resolve().await, None);
    }
    assert_eq!(target.resolve().await, None, "four stuck: no lookup");
    assert_eq!(resolver.started.load(Ordering::SeqCst), 4);
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_eq!(target.resolve().await, Some(addr()), "the stuck lookups ended");
}

/// A refresh looks the name up again; when the lookup finds nothing, the
/// addresses kept still serve until the next refresh. A forget drops them.
#[tokio::test]
async fn a_refresh_that_finds_nothing_serves_stale() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    resolver.point("limiter:8080", [addr()]);
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(addr()));
    resolver.unpoint("limiter:8080");
    target.refresh();
    assert_eq!(target.resolve().await, Some(addr()), "stale, served");
    assert_eq!(resolver.lookups(), 2, "the refresh looked the name up");
    assert_eq!(target.resolve().await, Some(addr()), "kept until the next refresh");
    assert_eq!(resolver.lookups(), 2);
    let moved: SocketAddr = "10.0.0.2:8080".parse().unwrap();
    resolver.point("limiter:8080", [moved]);
    target.refresh();
    assert_eq!(target.resolve().await, Some(moved), "a refresh that finds one takes it");
    resolver.unpoint("limiter:8080");
    target.forget();
    assert_eq!(target.resolve().await, None, "a forget drops them outright");
}

/// A refresh whose lookup hangs past its bound serves the stale addresses at
/// the bound.
#[tokio::test(start_paused = true)]
async fn a_refresh_past_its_bound_serves_stale() {
    let resolver = Arc::new(StuckResolver { stuck: usize::MAX, ..Default::default() });
    let target2 = ResolvedTarget::name_with_lookup(
        "limiter:8080",
        resolver.clone(),
        LookupSettings { bound: std::time::Duration::from_millis(300), max_stuck: 1 },
    );
    // The resolver never answers: the kept address is seeded by hand.
    if let Kind::Name(named) = &target2.kind {
        named.lock().addrs = Some(Arc::from([addr()]));
    }
    target2.refresh();
    let start = tokio::time::Instant::now();
    assert_eq!(target2.resolve().await, Some(addr()), "stale, at the bound");
    assert_eq!(start.elapsed(), std::time::Duration::from_millis(300));
    target2.refresh();
    let start = tokio::time::Instant::now();
    assert_eq!(target2.resolve().await, Some(addr()), "one stuck: stale at once");
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}
