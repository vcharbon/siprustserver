//! DNS resolution seam for **named** forward targets ([`HostResolver`] +
//! [`NamedForwarder`]).
//!
//! The proxy forwards almost exclusively to IP literals (registry workers, the
//! simulated fabric); the exception is a worker-outbound b-leg whose R-URI
//! carries a DNS name (e.g. a `sipp-uas` pod FQDN). Resolution must never run
//! on the recv-loop task: the loop is single-task, so one slow/unresolvable
//! name awaited inline head-of-line blocks ALL proxy traffic for the resolver
//! timeout — the starvation shape of a keepalive-burst collapse, but
//! sustained. The send path is therefore tiered:
//!
//! 1. **IP literal** — sent inline by the core, never touches this module.
//! 2. **Fresh positive cache hit** — sent inline here (lock-only, no await
//!    under the lock).
//! 3. **Anything else** — handed to a spawned single-flight resolve task; the
//!    recv loop never waits on DNS.
//!
//! The cache is bounded and TTL'd in both directions:
//! - a **positive TTL** so a restarted callee pod's new A record is picked up
//!   (a forever-cache would pin the dead IP for the process lifetime);
//! - a **negative TTL** so an unresolvable name costs one lookup per window,
//!   not one per packet;
//! - a **size cap**, because the `;outbound` self-Route lets any sender steer
//!   arbitrary R-URI hostnames into resolution — uncapped, each distinct name
//!   would be a permanent map entry (remote memory growth).
//!
//! Packets that race an in-flight resolve for the same name are dropped (and
//! counted).
//!
//! Cold-cache latency is attacked from three sides, so the
//! FIRST b-leg forward after a deploy / CoreDNS restart / TTL expiry is
//! warm-equivalent instead of a 3.5–7.5 s stall that blows the downstream 2 s
//! connect timer:
//! - **Absolute-first resolution** (`resolution_candidates`): kube ships
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
/// absolutely-qualified first (see `resolution_candidates`).
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
        Self {
            positive_ttl_ms: 60_000,
            negative_ttl_ms: 5_000,
            max_entries: 4096,
            max_in_flight: 64,
        }
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
        let ttl =
            if outcome.is_some() { self.cfg.positive_ttl_ms } else { self.cfg.negative_ttl_ms };
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.cfg.max_entries && !entries.contains_key(key) {
            entries.retain(|_, e| e.expires_at_ms > now);
            if entries.len() >= self.cfg.max_entries {
                if let Some(victim) = entries.keys().next().cloned() {
                    entries.remove(&victim);
                }
            }
        }
        entries.insert(
            key.to_string(),
            CacheEntry { outcome, expires_at_ms: now + ttl, used_since_refresh: false },
        );
        entries.len()
    }
}

/// Outcome labels for `sip_proxy_named_sends_total{outcome}`.
pub(crate) mod outcome {
    pub const CACHED: &str = "cached";
    pub const RESOLVED: &str = "resolved";
    pub const RESOLVE_FAILED: &str = "resolve_failed";
    pub const DROPPED_NEGATIVE: &str = "dropped_negative";
    pub const DROPPED_IN_FLIGHT: &str = "dropped_in_flight";
    /// Every outcome.
    pub const ALL: [&str; 5] =
        [CACHED, RESOLVED, RESOLVE_FAILED, DROPPED_NEGATIVE, DROPPED_IN_FLIGHT];
}

/// Outcome labels for `sip_proxy_resolver_refresh_total{outcome}` (proactive
/// refresh + startup prewarm — closed set).
pub(crate) mod refresh_outcome {
    pub const REFRESHED: &str = "refreshed";
    pub const FAILED: &str = "failed";
    pub const IDLE_STOPPED: &str = "idle_stopped";
    pub const PREWARMED: &str = "prewarmed";
    pub const PREWARM_FAILED: &str = "prewarm_failed";
    /// Every outcome.
    pub const ALL: [&str; 5] = [REFRESHED, FAILED, IDLE_STOPPED, PREWARMED, PREWARM_FAILED];
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
    /// Resolution-failure aggregation keyed by name (ADR-0026): a name that
    /// stops resolving is ONE episode — rising edge, ~5 s summaries, falling
    /// edge with the totals — ended once the name resolves again for the idle
    /// window, so a name flapping does not print a pair of lines per packet.
    /// Names are wire-influenceable, so the set's own key bound is what keeps
    /// this bounded.
    resolve_failures: Arc<observe::WaveSet>,
}

impl Inner {
    /// Store a resolve outcome (publishing the cache-size gauge) and, for a
    /// positive entry, arm the proactive refresh one-shot.
    fn store_and_arm(self: &Arc<Self>, target: &ProxyAddr, outcome: Option<SocketAddr>) {
        let key = target.to_string();
        let size = self.cache.store(&key, outcome);
        self.metrics.set_resolver_cache_size(size as u64);
        match outcome {
            Some(_) => {
                if self.resolve_failures.is_active() {
                    self.resolve_failures.recovered(&key);
                }
                self.arm_refresh(target);
            }
            None => self.resolve_failures.record(&key, "failures", 1),
        }
    }

    /// The name-resolution failure log. A name flapping is a per-packet event
    /// class, so it is aggregated into an episode per name.
    fn failure_waves() -> Arc<observe::WaveSet> {
        observe::WaveSet::new(|name: &str, r: &observe::WaveReport| {
            tracing::info!(
                node = observe::node(),
                name,
                edge = %r.edge,
                elapsed_ms = r.elapsed_ms,
                totals = %r.tally,
                "name resolution failing"
            );
        })
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
            inner
                .resolve_off_path(&target, refresh_outcome::REFRESHED, refresh_outcome::FAILED)
                .await;
        });
    }

    /// Off-path resolve for refresh/prewarm: claim the single-flight slot,
    /// resolve, re-store + re-arm on success (`ok`); on failure (`fail`) keep
    /// the existing positive entry (it stays servable until its TTL — a
    /// transient DNS blip must not cold a warm name) and re-arm for a retry.
    /// If the name is already in flight (a concurrent miss-resolve will store
    /// and re-arm) or the in-flight cap is hot, just re-arm. Never sends.
    async fn resolve_off_path(
        self: &Arc<Self>,
        target: &ProxyAddr,
        ok: &'static str,
        fail: &'static str,
    ) {
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
                self.resolve_failures.record(&key, "failures", 1);
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
            resolve_failures: Inner::failure_waves(),
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
                    .resolve_off_path(
                        &target,
                        refresh_outcome::PREWARMED,
                        refresh_outcome::PREWARM_FAILED,
                    )
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
                    if in_flight.contains(&key) || in_flight.len() >= inner.cache.cfg.max_in_flight
                    {
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
mod tests;
