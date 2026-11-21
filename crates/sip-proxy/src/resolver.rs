//! DNS resolution seam for named forward targets ([`HostResolver`] and
//! [`NamedForwarder`]).
//!
//! The recv loop never waits on DNS. An IP literal is sent inline by the core
//! and never reaches this module; a fresh positive cache hit is sent inline
//! here; anything else is handed to a spawned single-flight resolve task.
//!
//! Each lookup is bounded (`lookup_timeout_ms`). At the bound the packets
//! waiting on it are dropped and a negative entry written: a packet leaves
//! within the bound of the lookup it waits on, or never. The lookup's slot
//! stays taken until the lookup itself returns (a getaddrinfo thread cannot be
//! cancelled), so at most `max_in_flight` lookup threads are stuck per
//! forwarder, and a lookup past its bound starts no further attempt. Of a
//! name's addresses the first of the egress socket's family is used and kept
//! for the cache lifetime. Only A/AAAA records are used (ADR-0046 §8, §9).
//!
//! The cache has a positive TTL, a negative TTL and a size cap. A packet that
//! races an in-flight resolve for its name waits for it, behind the packets
//! already waiting, and leaves in arrival order to the address the resolve
//! found. The wait is bounded per name (`max_waiting_per_name`); a packet past
//! it, or one racing a lookup already past its bound, is dropped
//! (`dropped_in_flight`). Packets waiting on a resolve that fails are each
//! counted under its outcome.
//!
//! A dotted name is queried absolutely qualified (`{host}.`) first, the raw
//! name as the fallback. A positive entry used since its last refresh is
//! re-resolved off-path before its TTL (margin min(ttl/4, 5 s)); an unused one
//! expires. The refresh set is capped at `max_entries`, names pinned by
//! [`NamedForwarder::prewarm`] exempt. Refresh and prewarm outcomes are counted
//! in `sip_proxy_resolver_refresh_total{outcome}`; a failed refresh keeps the
//! positive entry and stores no negative one.

mod cache;
mod config;
mod lookup;
mod outcomes;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_net::{SendErrorKind, UdpEndpoint};

use crate::addr::ProxyAddr;
use crate::observability::ProxyMetrics;
use crate::poison::{locked, RESOLVER_IN_FLIGHT, RESOLVER_PINS, RESOLVER_REFRESHES};

use cache::{CacheLookup, NameCache};
pub use config::ResolverConfig;
#[cfg(test)]
use lookup::{resolution_candidates, resolve_first};
pub use lookup::{HostResolver, SystemResolver};
pub(crate) use outcomes::{outcome, refresh_outcome};

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
    /// Send `bytes` to `dst` now, counting a refusal and returning its kind.
    async fn send_inline(&self, bytes: &[u8], dst: SocketAddr) -> Option<SendErrorKind> {
        let refused = self.endpoint.send_to(bytes, dst).await.err()?;
        self.metrics.record_send_failure();
        Some(refused.kind)
    }

    /// Join `bytes` (a packet for the name `key`, or a refresh with none) to
    /// that name's resolve: claim one when none runs and the in-flight cap
    /// allows, else wait on the running one within the per-name bound. A
    /// packet finding no resolve reads the cache again first: a resolve
    /// publishes its outcome and frees its slot in one step under this lock
    /// ([`release`](Self::release)), so the packet sees one or the other.
    fn join(&self, key: &str, bytes: Option<&[u8]>) -> Join {
        let mut in_flight = locked(&self.in_flight, RESOLVER_IN_FLIGHT);
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
        let waiting =
            locked(&self.in_flight, RESOLVER_IN_FLIGHT).get_mut(key).and_then(Option::take);
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
                let mut in_flight = locked(&self.in_flight, RESOLVER_IN_FLIGHT);
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

    fn lookup_deadline(&self) -> tokio::time::Instant {
        sip_clock::deadline::from_now(Duration::from_millis(self.cache.cfg.lookup_timeout_ms))
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
            let pinned = locked(&self.pinned, RESOLVER_PINS).contains(&key);
            let mut armed = locked(&self.refresh_armed, RESOLVER_REFRESHES);
            if !pinned && armed.len() >= self.cache.cfg.max_entries {
                return;
            }
            if !armed.insert(key.clone()) {
                return;
            }
        }
        let (inner, target) = (self.clone(), target.clone());
        crate::supervise::spawn(crate::supervise::RESOLVER_LOOKUP, move || {
            inner.clone().refresh_when_due(target.clone(), key.clone())
        });
    }

    async fn refresh_when_due(self: Arc<Self>, target: ProxyAddr, key: String) {
        tokio::time::sleep(Duration::from_millis(self.cache.cfg.refresh_after_ms())).await;
        locked(&self.refresh_armed, RESOLVER_REFRESHES).remove(&key);
        let pinned = locked(&self.pinned, RESOLVER_PINS).contains(&key);
        let used = self.cache.take_used(&key).unwrap_or(false);
        if !pinned && !used {
            self.metrics.record_resolver_refresh(refresh_outcome::IDLE_STOPPED);
            return;
        }
        self.resolve_off_path(&target, refresh_outcome::REFRESHED, refresh_outcome::FAILED).await;
    }

    /// The lookup of a send that claimed the name's single flight: the
    /// waiting sends are released to what it finds, or timed out with it.
    async fn look_up_and_release(self: Arc<Self>, target: ProxyAddr, key: String) {
        let deadline = self.lookup_deadline();
        let mut lookup = self.resolver.resolve(&target.host, target.port, deadline);
        let Ok(addrs) = tokio::time::timeout_at(deadline, &mut lookup).await else {
            self.store_and_arm(&target, None);
            self.time_out_waiting(&key);
            lookup.await;
            self.release(&target, None, false).await;
            return;
        };
        let resolved = self.pick(addrs);
        self.release(&target, resolved, true).await;
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

/// A socket bound to `local` can send to `dst`: the same family, since an IPv6
/// socket is IPv6-only ([`sip_net::RealSignalingNetwork`]).
fn reaches(local: SocketAddr, dst: SocketAddr) -> bool {
    local.is_ipv4() == dst.is_ipv4()
}

/// The named-target send path: the inline cache hit and the spawned resolve of
/// the module doc. Cheap to clone.
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
            locked(&self.0.pinned, RESOLVER_PINS).insert(target.to_string());
            let inner = self.0.clone();
            let (ok, fail) = (refresh_outcome::PREWARMED, refresh_outcome::PREWARM_FAILED);
            crate::supervise::spawn(crate::supervise::RESOLVER_LOOKUP, move || {
                let (inner, target) = (inner.clone(), target.clone());
                async move { inner.resolve_off_path(&target, ok, fail).await }
            });
        }
    }

    /// Send `bytes` to a named target. Cache hits go out inline; a miss spawns
    /// a single-flight resolve-then-send task, or waits on the one running, so
    /// the caller (the recv loop) never waits on DNS. Every refusal is counted;
    /// the kind of one the socket answered inline is returned, and a send the
    /// resolve makes later returns `None`.
    pub async fn send(&self, bytes: &[u8], target: &ProxyAddr) -> Option<SendErrorKind> {
        let inner = &self.0;
        let key = target.to_string();
        match inner.cache.lookup(&key) {
            CacheLookup::Hit(dst) => {
                inner.metrics.record_named_send(outcome::CACHED);
                return inner.send_inline(bytes, dst).await;
            }
            CacheLookup::NegativeHit => {
                inner.metrics.record_named_send(outcome::DROPPED_NEGATIVE);
            }
            CacheLookup::Miss => match inner.join(&key, Some(bytes)) {
                Join::Waiting => {}
                Join::Cached(dst) => {
                    inner.metrics.record_named_send(outcome::CACHED);
                    return inner.send_inline(bytes, dst).await;
                }
                Join::Negative => inner.metrics.record_named_send(outcome::DROPPED_NEGATIVE),
                Join::Refused => inner.metrics.record_named_send(outcome::DROPPED_IN_FLIGHT),
                Join::Claimed => {
                    let (inner, target) = (self.0.clone(), target.clone());
                    crate::supervise::spawn(crate::supervise::RESOLVER_LOOKUP, move || {
                        inner.clone().look_up_and_release(target.clone(), key.clone())
                    });
                }
            },
        }
        None
    }
}

#[cfg(test)]
mod tests;
