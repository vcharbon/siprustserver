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
//!
//! Cold-cache latency (newkahneed-037) is attacked from three sides, so the
//! FIRST b-leg forward after a deploy / CoreDNS restart / TTL expiry is
//! warm-equivalent instead of a 3.5–7.5 s stall that blows the downstream 2 s
//! connect timer:
//! - **Absolute-first resolution** ([`resolution_candidates`]): kube ships
//!   `ndots:5`, so a dotted Service FQDN is search-domain-expanded through
//!   several serial lookups before being tried as-is. Querying the
//!   absolutely-qualified `{host}.` first collapses that to ONE lookup; the
//!   raw name stays as the fallback for names that genuinely need expansion.
//! - **Proactive refresh**: a positive entry that was USED since the last
//!   refresh is re-resolved off-path shortly *before* its TTL (margin =
//!   min(ttl/4, 5 s)), so an active name never expires cold. An entry that
//!   went unused stops refreshing and expires naturally — no unbounded churn
//!   for one-shot names. The armed set is capped at `max_entries` (names are
//!   wire-influenceable; a name-flood must not pin unbounded sleeping tasks) —
//!   pinned names are exempt from the cap.
//! - **Prewarm + pin** ([`NamedForwarder::prewarm`], `PROXY_RESOLVER_PREWARM`):
//!   operator-configured targets are resolved at startup (off the serving
//!   path, never blocking boot) and pinned, so they refresh every cycle
//!   regardless of use and stay permanently warm.
//!
//! Refresh/prewarm outcomes are counted in
//! `sip_proxy_resolver_refresh_total{outcome}`. A failed refresh never
//! clobbers the still-valid positive entry (it keeps serving until its TTL)
//! and never stores a negative entry — it just retries at the same cadence.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Candidate query strings for `host`, in attempt order. Pure — unit-tested.
///
/// Kubernetes resolv.conf ships `ndots:5`, so a Service FQDN like
/// `svc.ns.svc.cluster.local` (4 dots < 5) is FIRST expanded through every
/// search domain — several serial lookups (observed 3.5–7.5 s against cold
/// CoreDNS) before the name is tried as-is. Appending the root dot makes the
/// name absolute, which getaddrinfo resolves in ONE lookup:
/// - dotted and not already absolute → `["{host}.", host]` (the raw fallback
///   keeps short names that genuinely need search expansion working);
/// - single-label (no dot — only search expansion can resolve it) or already
///   absolute → `[host]`.
pub(crate) fn resolution_candidates(host: &str) -> Vec<String> {
    if host.contains('.') && !host.ends_with('.') {
        vec![format!("{host}."), host.to_string()]
    } else {
        vec![host.to_string()]
    }
}

/// Try each of [`resolution_candidates`] in order through `lookup`; the first
/// answer wins. Factored out of [`SystemResolver`] so the attempt ordering is
/// testable without real getaddrinfo calls.
async fn resolve_first<F, Fut>(host: &str, port: u16, lookup: F) -> Option<SocketAddr>
where
    F: Fn(String, u16) -> Fut,
    Fut: std::future::Future<Output = Option<SocketAddr>>,
{
    for candidate in resolution_candidates(host) {
        if let Some(addr) = lookup(candidate, port).await {
            return Some(addr);
        }
    }
    None
}

/// The production resolver — `tokio::net::lookup_host` (getaddrinfo on the
/// blocking pool), first result wins. A per-pod name is single-A, so it
/// resolves to ONE pod consistently. Dotted names are queried
/// absolutely-qualified first (see [`resolution_candidates`] — the ndots fix).
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Option<SocketAddr> {
        resolve_first(host, port, |candidate, port| async move {
            tokio::net::lookup_host((candidate.as_str(), port)).await.ok()?.next()
        })
        .await
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

impl ResolverConfig {
    /// When the proactive refresh fires, relative to a positive store:
    /// `positive_ttl − margin`, margin = min(ttl/4, 5 s). Both the margin and
    /// the delay are clamped ≥ 1 ms so degenerate tiny TTLs still schedule
    /// (a refresh landing at/after expiry idle-stops harmlessly).
    fn refresh_after_ms(&self) -> u64 {
        let margin = (self.positive_ttl_ms / 4).min(5_000).max(1);
        self.positive_ttl_ms.saturating_sub(margin).max(1)
    }
}

/// One cached resolution outcome. `None` = negative entry (lookup failed).
struct CacheEntry {
    outcome: Option<SocketAddr>,
    expires_at_ms: u64,
    /// Set on every cache Hit; the proactive refresh reads-and-clears it to
    /// decide refresh-vs-idle-stop (see the module doc).
    used_since_refresh: bool,
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
        match entries.get_mut(key) {
            Some(e) if e.expires_at_ms <= now => {
                entries.remove(key);
                CacheLookup::Miss
            }
            Some(e) => match e.outcome {
                Some(addr) => {
                    e.used_since_refresh = true;
                    CacheLookup::Hit(addr)
                }
                None => CacheLookup::NegativeHit,
            },
            None => CacheLookup::Miss,
        }
    }

    /// Read-and-clear the used-since-refresh flag for the refresh decision.
    /// `None` when there is no live positive entry (missing, expired, or
    /// negative) — the refresh treats that like "unused".
    fn take_used(&self, key: &str) -> Option<bool> {
        let now = self.now_ms();
        let mut entries = self.entries.lock().unwrap();
        let e = entries.get_mut(key)?;
        if e.expires_at_ms <= now || e.outcome.is_none() {
            return None;
        }
        let used = e.used_since_refresh;
        e.used_since_refresh = false;
        Some(used)
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
        entries.insert(key.to_string(), CacheEntry { outcome, expires_at_ms: now + ttl, used_since_refresh: false });
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

/// Outcome labels for `sip_proxy_resolver_refresh_total{outcome}` (proactive
/// refresh + startup prewarm — closed set).
mod refresh_outcome {
    pub const REFRESHED: &str = "refreshed";
    pub const FAILED: &str = "failed";
    pub const IDLE_STOPPED: &str = "idle_stopped";
    pub const PREWARMED: &str = "prewarmed";
    pub const PREWARM_FAILED: &str = "prewarm_failed";
}

struct Inner {
    endpoint: Arc<dyn UdpEndpoint>,
    resolver: Arc<dyn HostResolver>,
    cache: NameCache,
    /// Names with a resolve task currently running — the single-flight set.
    in_flight: Mutex<HashSet<String>>,
    /// Names pinned by [`NamedForwarder::prewarm`] — refreshed every cycle
    /// regardless of use (permanently warm).
    pinned: Mutex<HashSet<String>>,
    /// Names with a scheduled refresh one-shot — the duplicate-arm guard.
    /// Survives cache eviction on purpose (the pending task is the resource
    /// being guarded, not the entry). Capped at `max_entries` for non-pinned
    /// names (wire-influenceable; see module doc).
    refresh_armed: Mutex<HashSet<String>>,
    metrics: Arc<ProxyMetrics>,
}

impl Inner {
    /// Store a resolve outcome (publishing the cache-size gauge) and, for a
    /// positive entry, arm the proactive refresh one-shot.
    fn store_and_arm(self: &Arc<Self>, target: &ProxyAddr, outcome: Option<SocketAddr>) {
        let size = self.cache.store(&target.to_string(), outcome);
        self.metrics.set_resolver_cache_size(size as u64);
        if outcome.is_some() {
            self.arm_refresh(target);
        }
    }

    /// Schedule the per-name refresh one-shot at `refresh_after_ms` from now.
    /// No-op if one is already scheduled (duplicate guard) or the armed set is
    /// at the flood cap (pinned names are exempt). On firing: a used-or-pinned
    /// name re-resolves off-path and re-arms; an idle name stops refreshing
    /// and its entry expires naturally.
    fn arm_refresh(self: &Arc<Self>, target: &ProxyAddr) {
        let key = target.to_string();
        {
            let pinned = self.pinned.lock().unwrap().contains(&key);
            let mut armed = self.refresh_armed.lock().unwrap();
            if !pinned && armed.len() >= self.cache.cfg.max_entries {
                return; // flood guard: names beyond the cache cap get no proactive refresh
            }
            if !armed.insert(key.clone()) {
                return;
            }
        }
        let inner = self.clone();
        let target = target.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(inner.cache.cfg.refresh_after_ms())).await;
            inner.refresh_armed.lock().unwrap().remove(&key);
            let pinned = inner.pinned.lock().unwrap().contains(&key);
            let used = inner.cache.take_used(&key).unwrap_or(false);
            if !pinned && !used {
                // One-shot name: stop refreshing; the entry (if still there)
                // expires at its natural TTL — bounded churn.
                inner.metrics.record_resolver_refresh(refresh_outcome::IDLE_STOPPED);
                return;
            }
            inner.resolve_off_path(&target, refresh_outcome::REFRESHED, refresh_outcome::FAILED).await;
        });
    }

    /// Off-path resolve for refresh/prewarm: claim the single-flight slot,
    /// resolve, re-store + re-arm on success (`ok`); on failure (`fail`) keep
    /// the existing positive entry (it stays servable until its TTL — a
    /// transient DNS blip must not cold a warm name) and re-arm for a retry.
    /// If the name is already in flight (a concurrent miss-resolve will store
    /// and re-arm) or the in-flight cap is hot, just re-arm. Never sends.
    async fn resolve_off_path(self: &Arc<Self>, target: &ProxyAddr, ok: &'static str, fail: &'static str) {
        let key = target.to_string();
        let claimed = {
            let mut in_flight = self.in_flight.lock().unwrap();
            if in_flight.contains(&key) || in_flight.len() >= self.cache.cfg.max_in_flight {
                false
            } else {
                in_flight.insert(key.clone());
                true
            }
        };
        if !claimed {
            self.arm_refresh(target);
            return;
        }
        let resolved = self.resolver.resolve(&target.host, target.port).await;
        self.in_flight.lock().unwrap().remove(&key);
        match resolved {
            Some(_) => {
                self.metrics.record_resolver_refresh(ok);
                self.store_and_arm(target, resolved);
            }
            None => {
                self.metrics.record_resolver_refresh(fail);
                self.arm_refresh(target);
            }
        }
    }
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
            pinned: Mutex::new(HashSet::new()),
            refresh_armed: Mutex::new(HashSet::new()),
            metrics,
        }))
    }

    /// Pin `targets` permanently warm (`PROXY_RESOLVER_PREWARM`): resolve each
    /// now, off the serving path, and keep it in the proactive-refresh set
    /// regardless of use — so the FIRST send after a deploy / CoreDNS restart
    /// / TTL expiry is a cache hit, not a cold resolve. Never blocks the
    /// caller (boot must not wait on DNS); a failed prewarm stores nothing
    /// (the send path behaves exactly as without prewarm) and retries at the
    /// refresh cadence. Outcomes land in
    /// `sip_proxy_resolver_refresh_total{outcome=prewarmed|prewarm_failed}`.
    pub fn prewarm(&self, targets: Vec<ProxyAddr>) {
        for target in targets {
            self.0.pinned.lock().unwrap().insert(target.to_string());
            let inner = self.0.clone();
            tokio::spawn(async move {
                inner
                    .resolve_off_path(&target, refresh_outcome::PREWARMED, refresh_outcome::PREWARM_FAILED)
                    .await;
            });
        }
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
                if inner.endpoint.send_to(bytes, dst).await.is_err() {
                    inner.metrics.record_send_failure();
                }
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
                let target = target.clone();
                tokio::spawn(async move {
                    let resolved = inner.resolver.resolve(&target.host, target.port).await;
                    inner.store_and_arm(&target, resolved);
                    inner.in_flight.lock().unwrap().remove(&key);
                    match resolved {
                        Some(dst) => {
                            inner.metrics.record_named_send(outcome::RESOLVED);
                            if inner.endpoint.send_to(&bytes, dst).await.is_err() {
                                inner.metrics.record_send_failure();
                            }
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

    // ── absolute-first resolution (the kube ndots:5 fix) ────────────────────

    /// Pure candidate ordering: dotted names try the absolutely-qualified form
    /// first (ONE lookup instead of ndots search expansion), falling back to
    /// the raw name; single-label and already-absolute names go straight to
    /// one raw attempt.
    #[test]
    fn resolution_candidates_absolute_first_for_dotted_hosts() {
        assert_eq!(
            resolution_candidates("uas.sip-test.svc.cluster.local"),
            vec!["uas.sip-test.svc.cluster.local.".to_string(), "uas.sip-test.svc.cluster.local".to_string()],
        );
        assert_eq!(resolution_candidates("mrf"), vec!["mrf".to_string()], "single-label skips straight to raw");
        assert_eq!(
            resolution_candidates("uas.example."),
            vec!["uas.example.".to_string()],
            "already-absolute names are not double-dotted"
        );
    }

    /// A lookup-recording double for [`resolve_first`]: scripts which query
    /// strings answer, and records every attempt in order.
    fn scripted_lookup(
        answers: &'static [&'static str],
    ) -> (Arc<Mutex<Vec<String>>>, impl Fn(String, u16) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<SocketAddr>>>>)
    {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let recorded = attempts.clone();
        let lookup = move |candidate: String, _port: u16| {
            recorded.lock().unwrap().push(candidate.clone());
            let answer = answers.contains(&candidate.as_str()).then(dst);
            Box::pin(async move { answer }) as std::pin::Pin<Box<dyn std::future::Future<Output = Option<SocketAddr>>>>
        };
        (attempts, lookup)
    }

    #[tokio::test]
    async fn resolve_first_tries_the_absolute_form_first() {
        let (attempts, lookup) = scripted_lookup(&["uas.example."]);
        assert_eq!(resolve_first("uas.example", 5060, lookup).await, Some(dst()));
        assert_eq!(*attempts.lock().unwrap(), vec!["uas.example.".to_string()], "absolute answered — one lookup, no raw attempt");
    }

    #[tokio::test]
    async fn resolve_first_falls_back_to_the_raw_name_when_absolute_fails() {
        // The absolute form yields nothing (a short name that NEEDS kube
        // search-domain expansion) — the raw name is attempted second and wins.
        let (attempts, lookup) = scripted_lookup(&["uas.example"]);
        assert_eq!(resolve_first("uas.example", 5060, lookup).await, Some(dst()));
        assert_eq!(
            *attempts.lock().unwrap(),
            vec!["uas.example.".to_string(), "uas.example".to_string()],
            "fallback order must be absolute → raw"
        );
    }

    #[tokio::test]
    async fn resolve_first_single_label_goes_straight_to_raw() {
        let (attempts, lookup) = scripted_lookup(&["mrf"]);
        assert_eq!(resolve_first("mrf", 5060, lookup).await, Some(dst()));
        assert_eq!(*attempts.lock().unwrap(), vec!["mrf".to_string()]);
    }

    // ── proactive refresh + prewarm (newkahneed-037) ────────────────────────
    //
    // Default config: positive TTL 60 s, margin = min(60 s/4, 5 s) = 5 s, so
    // the refresh one-shot fires at exactly 55 s after a positive store. Each
    // `advance` below moves to exactly ONE pending deadline (test-clock rule 2).

    #[tokio::test(start_paused = true)]
    async fn used_entry_is_refreshed_before_ttl_and_stays_warm() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("uas.example", 5060);

        fwd.send(b"pkt1", &target).await; // miss → resolve, positive store at t=0
        settle().await;
        fwd.send(b"pkt2", &target).await; // cache hit — marks the entry USED
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

        // Exactly the refresh deadline (55 s): the used entry re-resolves
        // off-path and re-arms; nothing is sent by the refresh itself.
        tokio::time::advance(Duration::from_millis(55_000)).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "used entry must refresh before its TTL");
        assert_eq!(metrics.resolver_refresh_count("refreshed"), 1);
        assert_eq!(ep.sent.lock().unwrap().len(), 2, "refresh must not send anything");

        // Past the ORIGINAL 60 s TTL: still a warm hit — no cold miss, no
        // dropped or delayed datagram.
        tokio::time::advance(Duration::from_millis(6_000)).await;
        fwd.send(b"pkt3", &target).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "send after the old TTL must be a cache hit");
        assert_eq!(metrics.named_send_count("cached"), 2);
        assert_eq!(metrics.named_send_count("dropped_in_flight"), 0);
        assert_eq!(metrics.named_send_count("dropped_negative"), 0);
        assert_eq!(ep.sent.lock().unwrap().len(), 3, "the post-TTL datagram goes out inline");
    }

    #[tokio::test(start_paused = true)]
    async fn unused_entry_refresh_idle_stops_and_expires_normally() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("oneshot.example", 5060);

        fwd.send(b"pkt1", &target).await; // stored at t=0, never hit again
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

        // Refresh deadline: the entry was never USED → idle stop, no resolve.
        tokio::time::advance(Duration::from_millis(55_000)).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "an unused entry must not be refreshed");
        assert_eq!(metrics.resolver_refresh_count("idle_stopped"), 1);
        assert_eq!(metrics.resolver_refresh_count("refreshed"), 0);

        // The entry expires at its natural TTL; the next send is an ordinary
        // cold miss (bounded churn: exactly one refresh decision per store).
        tokio::time::advance(Duration::from_millis(5_001)).await;
        fwd.send(b"pkt2", &target).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "the idle-stopped entry must expire normally");
        assert_eq!(metrics.resolver_refresh_count("idle_stopped"), 1, "no further refresh churn before the new store's cycle");
    }

    #[tokio::test(start_paused = true)]
    async fn prewarmed_name_first_send_is_a_cache_hit() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("mrf.sip-test.svc.cluster.local", 6001);

        fwd.prewarm(vec![target.clone()]);
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "prewarm resolves off the serving path");
        assert_eq!(metrics.resolver_refresh_count("prewarmed"), 1);

        // The FIRST send is warm: outcome=cached, datagram out inline, zero
        // resolves on the send path.
        fwd.send(b"pkt1", &target).await;
        assert_eq!(metrics.named_send_count("cached"), 1, "first send after prewarm must be a cache hit");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "the send path must trigger no resolve");
        assert_eq!(ep.sent.lock().unwrap().len(), 1, "the datagram goes out immediately (no resolve wait)");
        assert_eq!(ep.sent.lock().unwrap()[0].1, dst());
    }

    #[tokio::test(start_paused = true)]
    async fn pinned_name_keeps_refreshing_across_ttl_windows_without_sends() {
        let resolver = Arc::new(FakeResolver::answering(dst()));
        let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("mrf.sip-test.svc.cluster.local", 6001);

        fwd.prewarm(vec![target.clone()]);
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

        // Three full refresh cycles with ZERO sends: a pinned name refreshes
        // every cycle regardless of use (never idle-stops).
        for round in 1..=3u64 {
            tokio::time::advance(Duration::from_millis(55_000)).await;
            settle().await;
            assert_eq!(resolver.calls.load(Ordering::SeqCst), 1 + round, "pinned refresh round {round}");
        }
        assert_eq!(metrics.resolver_refresh_count("refreshed"), 3);
        assert_eq!(metrics.resolver_refresh_count("idle_stopped"), 0, "a pinned name must never idle-stop");

        // ~2.75 original TTLs later, still warm on the first send.
        fwd.send(b"pkt", &target).await;
        assert_eq!(metrics.named_send_count("cached"), 1);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_prewarm_retries_at_the_refresh_cadence_and_stores_nothing() {
        let resolver = Arc::new(FakeResolver::failing());
        let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
        let target = ProxyAddr::new("notyet.sip-test.svc.cluster.local", 6001);

        fwd.prewarm(vec![target.clone()]);
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(metrics.resolver_refresh_count("prewarm_failed"), 1);

        // Pinned → retried at the refresh cadence (boot must not stay cold
        // forever because CoreDNS was slow at prewarm time).
        tokio::time::advance(Duration::from_millis(55_000)).await;
        settle().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "pinned prewarm failure must retry");
        assert_eq!(metrics.resolver_refresh_count("failed"), 1);

        // A failed prewarm stores NO negative entry: the send path behaves
        // exactly as without prewarm (miss → its own single-flight resolve).
        fwd.send(b"pkt", &target).await;
        assert_eq!(metrics.named_send_count("dropped_negative"), 0, "prewarm failure must not poison the send path");
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
