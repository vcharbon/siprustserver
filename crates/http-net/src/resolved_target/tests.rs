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

/// A refresh looks the name up again behind the next request, which is
/// answered the addresses kept; when the lookup finds nothing they still
/// serve until the next refresh, when it finds others they replace them. A
/// forget drops them.
#[tokio::test(start_paused = true)]
async fn a_refresh_serves_stale_and_takes_what_its_lookup_finds() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    resolver.point("limiter:8080", [addr()]);
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(addr()));
    resolver.unpoint("limiter:8080");
    target.refresh();
    assert_eq!(target.resolve().await, Some(addr()), "stale, served at once");
    sip_clock::testkit::settle().await;
    assert_eq!(resolver.lookups(), 2, "the refresh looked the name up");
    assert_eq!(target.resolve().await, Some(addr()), "kept until the next refresh");
    assert_eq!(resolver.lookups(), 2);
    let moved: SocketAddr = "10.0.0.2:8080".parse().unwrap();
    resolver.point("limiter:8080", [moved]);
    target.refresh();
    assert_eq!(target.resolve().await, Some(addr()), "stale while the lookup runs");
    sip_clock::testkit::settle().await;
    assert_eq!(target.resolve().await, Some(moved), "the lookup's find, from then on");
    resolver.unpoint("limiter:8080");
    target.forget();
    assert_eq!(target.resolve().await, None, "a forget drops them outright");
}

/// A refresh whose lookup hangs serves the stale addresses at once, the
/// lookup running past its bound behind the requests; once the cap of stuck
/// lookups is reached, a refresh starts none.
#[tokio::test(start_paused = true)]
async fn a_refresh_whose_lookup_hangs_serves_stale_at_once() {
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
    assert_eq!(target2.resolve().await, Some(addr()), "stale, at once");
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(resolver.started.load(Ordering::SeqCst), 1, "one lookup, now stuck");
    target2.refresh();
    assert_eq!(target2.resolve().await, Some(addr()), "one stuck: stale, no new lookup");
    sip_clock::testkit::settle().await;
    assert_eq!(resolver.started.load(Ordering::SeqCst), 1);
}

fn kept() -> SocketAddr {
    "192.0.2.1:8080".parse().unwrap()
}

fn moved() -> SocketAddr {
    "192.0.2.2:8080".parse().unwrap()
}

/// A fresh lookup runs with an address kept, and what it finds replaces it
/// at once, for the caller and for every request after.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_returns_and_keeps_what_it_finds() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    resolver.point("limiter:8080", [kept()]);
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(kept()));
    resolver.point("limiter:8080", [moved()]);
    assert_eq!(target.resolve_fresh_or_kept().await.as_deref(), Some(&[moved()][..]));
    assert_eq!(target.resolve().await, Some(moved()), "kept for the next request");
    assert_eq!(resolver.lookups(), 2);
}

/// A fresh lookup that finds nothing answers the addresses kept, which stay.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_that_finds_nothing_returns_the_kept_addresses() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    resolver.point("limiter:8080", [kept()]);
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(kept()));
    resolver.unpoint("limiter:8080");
    assert_eq!(target.resolve_fresh_or_kept().await.as_deref(), Some(&[kept()][..]));
    assert_eq!(resolver.lookups(), 2, "the name was looked up");
    assert_eq!(target.address(), Some(kept()));
}

/// A fresh lookup past its bound answers the addresses kept at the bound;
/// its late find is dropped.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_past_its_bound_returns_the_kept_addresses_at_the_bound() {
    let resolver = Arc::new(ScriptedResolver::default());
    *resolver.script.lock().unwrap() = [(0, Some(kept())), (10_000, Some(moved()))].into();
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(kept()));
    let start = tokio::time::Instant::now();
    assert_eq!(target.resolve_fresh_or_kept().await.as_deref(), Some(&[kept()][..]));
    assert_eq!(start.elapsed(), LOOKUP_BOUND);
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_eq!(target.address(), Some(kept()), "the late find is dropped");
}

/// With the cap of stuck lookups reached, a fresh lookup starts none and
/// answers the addresses kept at once.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_at_the_stuck_cap_returns_the_kept_addresses_at_once() {
    let resolver = Arc::new(ScriptedResolver::default());
    *resolver.script.lock().unwrap() = [(0, Some(kept())), (10_000, None)].into();
    let target = ResolvedTarget::name_with_lookup(
        "limiter:8080",
        resolver.clone(),
        LookupSettings { bound: std::time::Duration::from_millis(300), max_stuck: 1 },
    );
    assert_eq!(target.resolve().await, Some(kept()));
    assert_eq!(target.resolve_fresh_or_kept().await, Some(Arc::from([kept()])), "past its bound");
    let start = tokio::time::Instant::now();
    assert_eq!(target.resolve_fresh_or_kept().await, Some(Arc::from([kept()])), "one stuck");
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    assert_eq!(resolver.started.load(Ordering::SeqCst), 2, "no third lookup started");
}

/// A fresh lookup joins the one in flight rather than start another.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_joins_the_lookup_in_flight() {
    let resolver = Arc::new(ScriptedResolver::default());
    *resolver.script.lock().unwrap() = [(0, Some(kept())), (100, Some(moved()))].into();
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve().await, Some(kept()));
    target.refresh();
    target.look_up();
    let start = tokio::time::Instant::now();
    assert_eq!(target.resolve_fresh_or_kept().await, Some(Arc::from([moved()])));
    assert_eq!(start.elapsed(), std::time::Duration::from_millis(100), "the one in flight");
    assert_eq!(resolver.started.load(Ordering::SeqCst), 2);
}

/// A name with nothing kept whose fresh lookup finds nothing has no address.
#[tokio::test(start_paused = true)]
async fn a_fresh_lookup_with_nothing_kept_and_nothing_found_is_none() {
    let resolver = Arc::new(FakeResolver::default());
    let target = ResolvedTarget::name_with("limiter:8080", resolver.clone());
    assert_eq!(target.resolve_fresh_or_kept().await, None);
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1);
}

async fn resolve_all(target: Arc<ResolvedTarget>) -> Option<Addresses> {
    target.resolve_all().await
}

async fn resolve_fresh_or_kept(target: Arc<ResolvedTarget>) -> Option<Addresses> {
    target.resolve_fresh_or_kept().await
}

/// Polls one `request` of `target` once on a thread of its own, inside
/// `runtime` when one is given: what the poll came back with, or `None` when
/// the thread did not come back within the bound.
fn poll_once_on_a_thread<F>(
    target: Arc<ResolvedTarget>,
    runtime: Option<tokio::runtime::Handle>,
    request: fn(Arc<ResolvedTarget>) -> F,
) -> Option<std::thread::Result<std::task::Poll<Option<Addresses>>>>
where
    F: std::future::Future<Output = Option<Addresses>> + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lookup = std::pin::pin!(request(target));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            std::future::Future::poll(lookup.as_mut(), &mut cx)
        }));
        let _ = tx.send(polled);
    });
    rx.recv_timeout(std::time::Duration::from_secs(30)).ok()
}

/// A target whose resolver knows `limiter:8080`.
fn known_target() -> (Arc<FakeResolver>, Arc<ResolvedTarget>) {
    let resolver = Arc::new(FakeResolver::default());
    resolver.names.lock().unwrap().insert("limiter:8080".into(), addr());
    let target = Arc::new(ResolvedTarget::name_with("limiter:8080", resolver.clone()));
    (resolver, target)
}

/// The next lookup of `target`, on a live runtime, resolves afresh.
fn resolves_afresh(resolver: &FakeResolver, target: &ResolvedTarget) {
    let live = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    assert_eq!(live.block_on(target.resolve()), Some(addr()), "the slot is free");
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 1, "one fresh resolve");
}

/// A runtime that is shut down drops a spawned lookup on the spawning
/// thread: the request gets no address at once, and the slot is freed.
#[test]
fn a_lookup_spawned_on_a_shut_down_runtime_frees_its_slot() {
    let (resolver, target) = known_target();
    let closed = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let handle = closed.handle().clone();
    drop(closed);
    for attempt in 0..2 {
        let polled = poll_once_on_a_thread(target.clone(), Some(handle.clone()), resolve_all)
            .expect("the request comes back, never deadlocks");
        assert!(
            matches!(polled, Ok(std::task::Poll::Ready(None))),
            "attempt {attempt}: no address, at once"
        );
    }
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 0, "no lookup ran");
    resolves_afresh(&resolver, &target);
}

/// Outside any runtime the spawn is refused: the request panics at once,
/// and the slot is freed.
#[test]
fn a_lookup_started_outside_a_runtime_frees_its_slot() {
    let (resolver, target) = known_target();
    let polled = poll_once_on_a_thread(target.clone(), None, resolve_all)
        .expect("the request comes back, never deadlocks");
    assert!(polled.is_err(), "a spawn outside a runtime panics");
    assert_eq!(resolver.lookups.load(Ordering::SeqCst), 0, "no lookup ran");
    resolves_afresh(&resolver, &target);
}

/// A fresh lookup whose task ends without answering (spawned on a runtime
/// that is shut down, which drops it at once) answers the addresses kept at
/// once, and frees the slot for the next lookup.
#[test]
fn a_fresh_lookup_whose_task_is_dropped_returns_the_kept_addresses() {
    let resolver = Arc::new(crate::MemoryResolver::new());
    resolver.point("limiter:8080", [kept()]);
    let target = Arc::new(ResolvedTarget::name_with("limiter:8080", resolver.clone()));
    let live = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    assert_eq!(live.block_on(target.resolve()), Some(kept()));
    let closed = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let handle = closed.handle().clone();
    drop(closed);

    let polled = poll_once_on_a_thread(target.clone(), Some(handle), resolve_fresh_or_kept)
        .expect("the request comes back, never deadlocks");
    let Ok(std::task::Poll::Ready(answer)) = polled else {
        panic!("answered at once, without a panic");
    };
    assert_eq!(answer.as_deref(), Some(&[kept()][..]), "the addresses kept");
    assert_eq!(resolver.lookups(), 1, "the dropped lookup never ran");

    resolver.point("limiter:8080", [moved()]);
    assert_eq!(
        live.block_on(target.resolve_fresh_or_kept()).as_deref(),
        Some(&[moved()][..]),
        "the slot is free: the next fresh lookup runs"
    );
}
