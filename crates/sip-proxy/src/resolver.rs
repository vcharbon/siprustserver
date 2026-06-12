//! DNS resolution seam for **named** forward targets ([`HostResolver`] +
//! [`NamedForwarder`]).
//!
//! The proxy forwards almost exclusively to IP literals (registry workers, the
//! simulated fabric); the exception is a worker-outbound b-leg whose R-URI
//! carries a DNS name (e.g. a `sipp-uas` pod FQDN). Resolution must never run
//! on the recv-loop task: the loop is single-task, so one slow/unresolvable
//! name awaited inline head-of-line blocks ALL proxy traffic for the resolver
//! timeout — the same starvation shape as the 2026-06-12 keepalive-burst
//! collapse, but sustained. The send path is therefore tiered:
//!
//! 1. **IP literal** — sent inline by the core, never touches this module.
//! 2. **Fresh positive cache hit** — sent inline here (lock-only, no await
//!    under the lock).
//! 3. **Anything else** — handed to a spawned single-flight resolve task; the
//!    recv loop never waits on DNS.
//!
//! The cache is bounded and TTL'd in both directions, replacing the old
//! process-global forever-cache:
//! - a **positive TTL** so a restarted callee pod's new A record is picked up
//!   (the old cache pinned the dead IP for the process lifetime);
//! - a **negative TTL** so an unresolvable name costs one lookup per window,
//!   not one per packet;
//! - a **size cap**, because the `;outbound` self-Route lets any sender steer
//!   arbitrary R-URI hostnames into resolution — uncapped, each distinct name
//!   was a permanent map entry (remote memory growth).
//!
//! Packets that race an in-flight resolve for the same name are dropped (and
//! counted): SIP retransmission covers the loss, and carrying waiters would
//! buy complexity for a window that is typically single-digit milliseconds.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;
use sip_net::UdpEndpoint;

use crate::addr::ProxyAddr;
use crate::observability::ProxyMetrics;

/// Resolve a `(host, port)` name to a socket address. Seam over the system
/// resolver so tests can inject failures/latency and assert call counts.
#[async_trait]
pub trait HostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Option<SocketAddr>;
}

/// The production resolver — `tokio::net::lookup_host` (getaddrinfo on the
/// blocking pool), first result wins. A per-pod name is single-A, so it
/// resolves to ONE pod consistently.
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Option<SocketAddr> {
        tokio::net::lookup_host((host, port)).await.ok()?.next()
    }
}

/// Cache + concurrency policy for [`NamedForwarder`].
#[derive(Debug, Clone, Copy)]
pub struct ResolverConfig {
    /// How long a successful resolution is served from cache.
    pub positive_ttl_ms: u64,
    /// How long a FAILED resolution short-circuits further lookups.
    pub negative_ttl_ms: u64,
    /// Cache size cap (names are wire-influenceable; see module doc).
    pub max_entries: usize,
    /// Max concurrent in-flight resolve tasks (each can park for the full
    /// resolver timeout, so this also bounds spawned-task buildup).
    pub max_in_flight: usize,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self { positive_ttl_ms: 60_000, negative_ttl_ms: 5_000, max_entries: 4096, max_in_flight: 64 }
    }
}

/// One cached resolution outcome. `None` = negative entry (lookup failed).
struct CacheEntry {
    outcome: Option<SocketAddr>,
    expires_at_ms: u64,
}

enum CacheLookup {
    Hit(SocketAddr),
    NegativeHit,
    Miss,
}

/// Bounded, two-sided-TTL name cache. Lock-only (`std::sync::Mutex`, never
/// held across an await).
struct NameCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    clock: Clock,
    cfg: ResolverConfig,
}

impl NameCache {
    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    fn lookup(&self, key: &str) -> CacheLookup {
        let now = self.now_ms();
        let mut entries = self.entries.lock().unwrap();
        match entries.get(key) {
            Some(e) if e.expires_at_ms <= now => {
                entries.remove(key);
                CacheLookup::Miss
            }
            Some(CacheEntry { outcome: Some(addr), .. }) => CacheLookup::Hit(*addr),
            Some(CacheEntry { outcome: None, .. }) => CacheLookup::NegativeHit,
            None => CacheLookup::Miss,
        }
    }

    /// Store an outcome (positive or negative TTL by kind); returns the new
    /// size for the gauge. At the cap, expired entries are reclaimed first,
    /// then an arbitrary live entry is evicted — new names must always be able
    /// to enter, else a name-flood would pin the cache against real targets.
    fn store(&self, key: &str, outcome: Option<SocketAddr>) -> usize {
        let now = self.now_ms();
        let ttl = if outcome.is_some() { self.cfg.positive_ttl_ms } else { self.cfg.negative_ttl_ms };
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.cfg.max_entries && !entries.contains_key(key) {
            entries.retain(|_, e| e.expires_at_ms > now);
            if entries.len() >= self.cfg.max_entries {
                if let Some(victim) = entries.keys().next().cloned() {
                    entries.remove(&victim);
                }
            }
        }
        entries.insert(key.to_string(), CacheEntry { outcome, expires_at_ms: now + ttl });
        entries.len()
    }
}

/// Outcome labels for `sip_proxy_named_sends_total{outcome}`.
mod outcome {
    pub const CACHED: &str = "cached";
    pub const RESOLVED: &str = "resolved";
    pub const RESOLVE_FAILED: &str = "resolve_failed";
    pub const DROPPED_NEGATIVE: &str = "dropped_negative";
    pub const DROPPED_IN_FLIGHT: &str = "dropped_in_flight";
}

struct Inner {
    endpoint: Arc<dyn UdpEndpoint>,
    resolver: Arc<dyn HostResolver>,
    cache: NameCache,
    /// Names with a resolve task currently running — the single-flight set.
    in_flight: Mutex<HashSet<String>>,
    metrics: Arc<ProxyMetrics>,
}

/// The named-target send path (tiers 2–3 of the module doc). Cheap to clone.
#[derive(Clone)]
pub struct NamedForwarder(Arc<Inner>);

impl NamedForwarder {
    pub fn new(
        endpoint: Arc<dyn UdpEndpoint>,
        resolver: Arc<dyn HostResolver>,
        cfg: ResolverConfig,
        clock: Clock,
        metrics: Arc<ProxyMetrics>,
    ) -> Self {
        Self(Arc::new(Inner {
            endpoint,
            resolver,
            cache: NameCache { entries: Mutex::new(HashMap::new()), clock, cfg },
            in_flight: Mutex::new(HashSet::new()),
            metrics,
        }))
    }

    /// Send `bytes` to a named target. Cache hits go out inline; a miss spawns
    /// a single-flight resolve-then-send task so the caller (the recv loop)
    /// never waits on DNS.
    pub async fn send(&self, bytes: &[u8], target: &ProxyAddr) {
        let inner = &self.0;
        let key = target.to_string();
        match inner.cache.lookup(&key) {
            CacheLookup::Hit(dst) => {
                inner.metrics.record_named_send(outcome::CACHED);
                let _ = inner.endpoint.send_to(bytes, dst).await;
            }
            CacheLookup::NegativeHit => {
                inner.metrics.record_named_send(outcome::DROPPED_NEGATIVE);
            }
            CacheLookup::Miss => {
                let claimed = {
                    let mut in_flight = inner.in_flight.lock().unwrap();
                    if in_flight.contains(&key) || in_flight.len() >= inner.cache.cfg.max_in_flight {
                        false
                    } else {
                        in_flight.insert(key.clone());
                        true
                    }
                };
                if !claimed {
                    inner.metrics.record_named_send(outcome::DROPPED_IN_FLIGHT);
                    return;
                }
                let inner = self.0.clone();
                let bytes = bytes.to_vec();
                let host = target.host.clone();
                let port = target.port;
                tokio::spawn(async move {
                    let resolved = inner.resolver.resolve(&host, port).await;
                    let size = inner.cache.store(&key, resolved);
                    inner.metrics.set_resolver_cache_size(size as u64);
                    inner.in_flight.lock().unwrap().remove(&key);
                    match resolved {
                        Some(dst) => {
                            inner.metrics.record_named_send(outcome::RESOLVED);
                            let _ = inner.endpoint.send_to(&bytes, dst).await;
                        }
                        None => inner.metrics.record_named_send(outcome::RESOLVE_FAILED),
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use sip_net::{SendError, UdpEndpointCounters, UdpPacket};

    /// Endpoint double that records every send.
    #[derive(Default)]
    struct CapturingEndpoint {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    #[async_trait]
    impl UdpEndpoint for CapturingEndpoint {
        async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
            self.sent.lock().unwrap().push((buf.to_vec(), dst));
            Ok(())
        }
        async fn recv(&self) -> Option<UdpPacket> {
            std::future::pending().await
        }
        fn try_recv(&self) -> Option<UdpPacket> {
            None
        }
        fn local_addr(&self) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
        }
        fn queue_depth(&self) -> usize {
            0
        }
        fn queue_max(&self) -> usize {
            0
        }
        fn counters(&self) -> UdpEndpointCounters {
            UdpEndpointCounters::default()
        }
    }

    /// Scriptable resolver: counts calls, optionally delays, then returns the
    /// configured outcome.
    struct FakeResolver {
        calls: AtomicU64,
        delay: Duration,
        answer: Option<SocketAddr>,
    }

    impl FakeResolver {
        fn answering(addr: SocketAddr) -> Self {
            Self { calls: AtomicU64::new(0), delay: Duration::ZERO, answer: Some(addr) }
        }
        fn failing() -> Self {
            Self { calls: AtomicU64::new(0), delay: Duration::ZERO, answer: None }
        }
        fn slow(addr: SocketAddr, delay: Duration) -> Self {
            Self { calls: AtomicU64::new(0), delay, answer: Some(addr) }
        }
    }

    #[async_trait]
    impl HostResolver for FakeResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> Option<SocketAddr> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.answer
        }
    }

    fn dst() -> SocketAddr {
        "10.9.9.9:5060".parse().unwrap()
    }

    fn forwarder(
        resolver: Arc<dyn HostResolver>,
        cfg: ResolverConfig,
    ) -> (NamedForwarder, Arc<CapturingEndpoint>, Arc<ProxyMetrics>) {
        let ep = Arc::new(CapturingEndpoint::default());
        let metrics = Arc::new(ProxyMetrics::new());
        let fwd = NamedForwarder::new(ep.clone(), resolver, cfg, Clock::test_at(0), metrics.clone());
        (fwd, ep, metrics)
    }

    async fn settle() {
        // Let the spawned resolve task run (and auto-advance past any delay).
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resolves_once_then_serves_from_cache() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("uas.example", 5060);

        fwd.send(b"pkt1", &target).await;
        settle().await;
        fwd.send(b"pkt2", &target).await;

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "second send must be a cache hit");
        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|(_, d)| *d == dst()));
        assert_eq!(metrics.named_send_count("resolved"), 1);
        assert_eq!(metrics.named_send_count("cached"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_lookup_is_negatively_cached_then_retried_after_ttl() {
        let resolver = Arc::new(FakeResolver::failing());
        let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("gone.example", 5060);

        fwd.send(b"pkt1", &target).await;
        settle().await;
        // Within the negative TTL: short-circuited, no second lookup.
        fwd.send(b"pkt2", &target).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.named_send_count("dropped_negative"), 1);

        // Past the negative TTL: a fresh lookup is allowed.
        tokio::time::advance(Duration::from_millis(5_001)).await;
        fwd.send(b"pkt3", &target).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert!(ep.sent.lock().unwrap().is_empty(), "nothing resolvable, nothing sent");
    }

    #[tokio::test(start_paused = true)]
    async fn positive_entry_expires_and_picks_up_new_address() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, _ep, _metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("uas.example", 5060);

        fwd.send(b"pkt1", &target).await;
        settle().await;
        tokio::time::advance(Duration::from_millis(60_001)).await;
        fwd.send(b"pkt2", &target).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "expired positive entry must re-resolve");
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_sends_to_same_name_are_single_flight() {
        let resolver = Arc::new(FakeResolver::slow(dst(), Duration::from_secs(2)));
        let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("uas.example", 5060);

        fwd.send(b"pkt1", &target).await;
        fwd.send(b"pkt2", &target).await; // races the in-flight resolve → dropped
        assert_eq!(metrics.named_send_count("dropped_in_flight"), 1);
        tokio::task::yield_now().await; // let the resolve task start (it parks at its delay)
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "single-flight: one lookup per name");

        tokio::time::advance(Duration::from_millis(2_001)).await;
        settle().await;
        assert_eq!(ep.sent.lock().unwrap().len(), 1, "the claiming packet is sent after resolve");
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_cap_bounds_spawned_resolves() {
        let resolver = Arc::new(FakeResolver::slow(dst(), Duration::from_secs(5)));
        let cfg = ResolverConfig { max_in_flight: 2, ..ResolverConfig::default() };
        let (fwd, _ep, metrics) = forwarder(resolver.clone(), cfg);

        for i in 0..5 {
            fwd.send(b"pkt", &ProxyAddr::new(format!("n{i}.example"), 5060)).await;
        }
        assert_eq!(metrics.named_send_count("dropped_in_flight"), 3);
        tokio::task::yield_now().await; // let the claimed tasks start (they park at their delay)
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "cap must bound concurrent resolves");
    }

    #[tokio::test(start_paused = true)]
    async fn cache_is_capped() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let cfg = ResolverConfig { max_entries: 3, max_in_flight: 64, ..ResolverConfig::default() };
        let (fwd, _ep, metrics) = forwarder(resolver.clone(), cfg);

        for i in 0..10 {
            fwd.send(b"pkt", &ProxyAddr::new(format!("n{i}.example"), 5060)).await;
            settle().await;
        }
        assert!(
            metrics.resolver_cache_size() <= 3,
            "cache must stay at the cap, got {}",
            metrics.resolver_cache_size()
        );
    }
}
