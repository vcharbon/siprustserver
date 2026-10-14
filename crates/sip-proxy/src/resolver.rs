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
//! Each lookup is bounded (`lookup_timeout_ms`, 2 s). At the bound the packets
//! waiting on it are dropped and a negative entry written: a packet leaves
//! within the bound of the lookup it waits on, or never, so a request never
//! leaves after its sender's transaction gave up. The
//! lookup's slot stays taken until the lookup itself returns (a getaddrinfo
//! thread cannot be cancelled), so at most `max_in_flight` lookup threads are
//! stuck per forwarder.
//!
//! A lookup past its bound starts no further attempt (no second name form):
//! its slot frees when the attempt already running returns. Of a name's
//! addresses, the first the egress socket can reach is used (the socket of
//! the face it leaves on; an unspecified IPv6 bind reaches both families),
//! and kept for the cache lifetime. Only A/AAAA records are used: no
//! NAPTR/SRV selection (RFC 3263 §4.1-4.2, ADR-0046).
//!
//! The cache is bounded and TTL'd in both directions:
//! - a **positive TTL** so a restarted callee pod's new A record is picked up
//!   (a forever-cache would pin the dead IP for the process lifetime);
//! - a **negative TTL** so an unresolvable name costs one lookup per window,
//!   not one per packet;
//! - a **size cap**, because every sender on the internal plane (any sender of
//!   a single-face proxy, ADR-0048) can steer arbitrary R-URI hostnames into
//!   resolution — uncapped, each distinct name would be a permanent map entry.
//!
//! A packet that races an in-flight resolve for its name waits for it, behind
//! the packets already waiting, and leaves in arrival order to the address the
//! resolve found: neither a 2xx ACK nor a non-INVITE response is retransmitted,
//! so a drop here would be a loss. The wait is bounded per name
//! (`max_waiting_per_name`); a packet past it, or one racing a lookup already
//! past its bound, is dropped (`dropped_in_flight`). Packets waiting on a
//! resolve that fails are each counted under its outcome.
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

mod cache;
mod lookup;
mod outcomes;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_net::UdpEndpoint;

use crate::addr::ProxyAddr;
use crate::observability::ProxyMetrics;

use cache::{CacheLookup, NameCache};
#[cfg(test)]
use lookup::{resolution_candidates, resolve_first};
pub use lookup::{HostResolver, SystemResolver};
pub(crate) use outcomes::{outcome, refresh_outcome};

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
    /// The bound on one name lookup (see the module doc).
    pub lookup_timeout_ms: u64,
    /// The most packets held on one name's in-flight resolve, the one that
    /// started it included.
    pub max_waiting_per_name: usize,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            positive_ttl_ms: 60_000,
            negative_ttl_ms: 5_000,
            max_entries: 4096,
            max_in_flight: 64,
            lookup_timeout_ms: 2_000,
            max_waiting_per_name: 32,
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

struct Inner {
    endpoint: Arc<dyn UdpEndpoint>,
    resolver: Arc<dyn HostResolver>,
    cache: NameCache,
    /// Names with a resolve task currently running — the single-flight set —
    /// each with the packets waiting on it in arrival order; `None` once the
    /// lookup passed its bound, when no packet waits on it any more.
    in_flight: Mutex<HashMap<String, Option<Vec<Vec<u8>>>>>,
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

/// What a packet missing the cache does about its name's resolve.
enum Join {
    /// It starts the resolve, and waits on it first.
    Claimed,
    /// It waits on the resolve already running.
    Waiting,
    /// A resolve published the address since the packet missed the cache.
    Cached(SocketAddr),
    /// A resolve published a failure since the packet missed the cache.
    Negative,
    /// No resolve can take it.
    Refused,
}

impl Inner {
    /// Join `bytes` (a packet for the name `key`, or a refresh with none) to
    /// that name's resolve: claim one when none runs and the in-flight cap
    /// allows, else wait on the running one within the per-name bound. A
    /// packet finding no resolve reads the cache again first: a resolve
    /// publishes its outcome and frees its slot in one step under this lock
    /// ([`release`](Self::release)), so the packet sees one or the other.
    fn join(&self, key: &str, bytes: Option<&[u8]>) -> Join {
        let mut in_flight = self.in_flight.lock().unwrap();
        if bytes.is_some() && !in_flight.contains_key(key) {
            match self.cache.lookup(key) {
                CacheLookup::Hit(dst) => return Join::Cached(dst),
                CacheLookup::NegativeHit => return Join::Negative,
                CacheLookup::Miss => {}
            }
        }
        let at_cap = in_flight.len() >= self.cache.cfg.max_in_flight;
        match in_flight.get_mut(key) {
            Some(Some(waiting)) if bytes.is_some() => {
                if waiting.len() >= self.cache.cfg.max_waiting_per_name {
                    return Join::Refused;
                }
                waiting.extend(bytes.map(<[u8]>::to_vec));
                Join::Waiting
            }
            Some(_) => Join::Refused,
            None if at_cap => Join::Refused,
            None => {
                let first = bytes.map(<[u8]>::to_vec).into_iter().collect();
                in_flight.insert(key.to_string(), Some(first));
                Join::Claimed
            }
        }
    }

    /// Fail the packets waiting on `key`'s lookup, which passed its bound,
    /// each counted `lookup_timed_out`; the slot stays taken until the lookup
    /// returns ([`release`](Self::release)).
    fn time_out_waiting(&self, key: &str) {
        let waiting = self.in_flight.lock().unwrap().get_mut(key).and_then(Option::take);
        for _ in waiting.unwrap_or_default() {
            self.metrics.record_named_send(outcome::LOOKUP_TIMED_OUT);
        }
    }

    /// Settle the packets waiting on `target`'s resolve, then free its slot:
    /// each is sent to `resolved` in arrival order, or counted
    /// `resolve_failed` without one, packets joining meanwhile included. Once
    /// the wait is empty the outcome is published (when `publish`) and the
    /// slot freed in one step under the in-flight lock, so no later packet
    /// overtakes a waiting one and none starts a second lookup in between.
    async fn release(
        self: &Arc<Self>,
        target: &ProxyAddr,
        resolved: Option<SocketAddr>,
        publish: bool,
    ) {
        let key = target.to_string();
        loop {
            let waiting = {
                let mut in_flight = self.in_flight.lock().unwrap();
                let waiting = in_flight
                    .get_mut(&key)
                    .and_then(|w| w.as_mut().map(std::mem::take))
                    .unwrap_or_default();
                if waiting.is_empty() {
                    if publish {
                        self.store_and_arm(target, resolved);
                    }
                    in_flight.remove(&key);
                    return;
                }
                waiting
            };
            for bytes in waiting {
                let Some(dst) = resolved else {
                    self.metrics.record_named_send(outcome::RESOLVE_FAILED);
                    continue;
                };
                self.metrics.record_named_send(outcome::RESOLVED);
                if self.endpoint.send_to(&bytes, dst).await.is_err() {
                    self.metrics.record_send_failure();
                }
            }
        }
    }

    /// When a lookup starting now reaches its bound.
    fn lookup_deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_millis(self.cache.cfg.lookup_timeout_ms)
    }

    /// The first of `addrs` the socket it would leave from can send to.
    fn pick(&self, addrs: Vec<SocketAddr>) -> Option<SocketAddr> {
        addrs.into_iter().find(|dst| reaches(self.endpoint.egress_local_addr(*dst), *dst))
    }

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
        observe::spawn_keeping_node(async move {
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
        if !matches!(self.join(&key, None), Join::Claimed) {
            self.arm_refresh(target);
            return;
        }
        let deadline = self.lookup_deadline();
        let mut lookup = self.resolver.resolve(&target.host, target.port, deadline);
        let resolved = match tokio::time::timeout_at(deadline, &mut lookup).await {
            Ok(addrs) => self.pick(addrs),
            Err(_) => {
                self.metrics.record_resolver_refresh(fail);
                self.resolve_failures.record(&key, "timeouts", 1);
                self.arm_refresh(target);
                self.time_out_waiting(&key);
                lookup.await;
                self.release(target, None, false).await;
                return;
            }
        };
        // A failed refresh keeps the positive entry: it publishes nothing.
        self.release(target, resolved, resolved.is_some()).await;
        match resolved {
            Some(_) => self.metrics.record_resolver_refresh(ok),
            None => {
                self.metrics.record_resolver_refresh(fail);
                self.resolve_failures.record(&key, "failures", 1);
                self.arm_refresh(target);
            }
        }
    }
}

/// A socket bound to `local` can send to `dst`: the same family, or an
/// unspecified IPv6 bind, which is dual-stack and takes both.
fn reaches(local: SocketAddr, dst: SocketAddr) -> bool {
    local.is_ipv4() == dst.is_ipv4() || (local.is_ipv6() && local.ip().is_unspecified())
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
            cache: NameCache::new(clock, cfg),
            in_flight: Mutex::new(HashMap::new()),
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
            observe::spawn_keeping_node(async move {
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
    /// a single-flight resolve-then-send task, or waits on the one running, so
    /// the caller (the recv loop) never waits on DNS.
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
            CacheLookup::Miss => match inner.join(&key, Some(bytes)) {
                Join::Waiting => {}
                Join::Cached(dst) => {
                    inner.metrics.record_named_send(outcome::CACHED);
                    if inner.endpoint.send_to(bytes, dst).await.is_err() {
                        inner.metrics.record_send_failure();
                    }
                }
                Join::Negative => inner.metrics.record_named_send(outcome::DROPPED_NEGATIVE),
                Join::Refused => inner.metrics.record_named_send(outcome::DROPPED_IN_FLIGHT),
                Join::Claimed => {
                    let inner = self.0.clone();
                    let target = target.clone();
                    observe::spawn_keeping_node(async move {
                        let deadline = inner.lookup_deadline();
                        let mut lookup =
                            inner.resolver.resolve(&target.host, target.port, deadline);
                        let Ok(addrs) = tokio::time::timeout_at(deadline, &mut lookup).await else {
                            inner.store_and_arm(&target, None);
                            inner.time_out_waiting(&key);
                            lookup.await;
                            inner.release(&target, None, false).await;
                            return;
                        };
                        let resolved = inner.pick(addrs);
                        inner.release(&target, resolved, true).await;
                    });
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
