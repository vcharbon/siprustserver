//! [`PoolGauges`] — the worker pool as the proxy sees it: how many workers
//! are in each health state and, alive, accept new calls, whether none is
//! routable, and the health of the discovery that fills the pool.

use std::sync::atomic::{AtomicU64, Ordering};

use super::ProxyMetrics;
use crate::observability::catalogue as c;

impl ProxyMetrics {
    /// Set the worker-health gauges from a fleet count. Also derives
    /// `worker_pool_empty` = `1` iff no worker is `Alive` (routable) — the
    /// routing-fatal condition the `/readyz` gate also keys on.
    pub fn set_worker_health_counts(&self, alive: u64, unknown: u64, dead: u64) {
        self.pool.set_health_counts([alive, unknown, dead]);
    }

    /// Set the worker-acceptance gauges: alive workers accepting and refusing
    /// new calls (ADR-0050).
    pub fn set_worker_acceptance_counts(&self, accepting: u64, refusing: u64) {
        self.pool.set_acceptance_counts([accepting, refusing]);
    }

    /// Publish the worker discovery's own health: whether its kube client is
    /// failing to build, and since when (Unix ms) its watch is broken.
    pub fn set_discovery(&self, no_client: bool, watch_broken_since_unix_ms: Option<u64>) {
        self.pool.set_discovery(no_client, watch_broken_since_unix_ms);
    }
}

/// The worker pool gauges, set by the runner's sampler.
#[derive(Debug, Default)]
pub(super) struct PoolGauges {
    /// Workers per health state: alive, unknown, dead.
    health: [AtomicU64; 3],
    /// Alive workers per acceptance of new calls: accept, refuse.
    acceptance: [AtomicU64; 2],
    /// `1` iff no worker is `Alive` (routable): the proxy can serve no new
    /// dialog. Pairs with the `/readyz` gate.
    empty: AtomicU64,
    /// `1` while worker discovery's kube client fails to build.
    no_client: AtomicU64,
    /// Unix time (ms) the worker watch broke, `0` while it delivers.
    watch_broken_since_ms: AtomicU64,
}

impl PoolGauges {
    pub(super) fn set_health_counts(&self, counts: [u64; 3]) {
        for (gauge, n) in self.health.iter().zip(counts) {
            gauge.store(n, Ordering::Relaxed);
        }
        self.empty.store(u64::from(counts[0] == 0), Ordering::Relaxed);
    }

    pub(super) fn set_acceptance_counts(&self, counts: [u64; 2]) {
        for (gauge, n) in self.acceptance.iter().zip(counts) {
            gauge.store(n, Ordering::Relaxed);
        }
    }

    pub(super) fn set_discovery(&self, no_client: bool, watch_broken_since_unix_ms: Option<u64>) {
        self.no_client.store(u64::from(no_client), Ordering::Relaxed);
        self.watch_broken_since_ms
            .store(watch_broken_since_unix_ms.unwrap_or(0), Ordering::Relaxed);
    }

    /// The pool-empty and discovery families.
    pub(super) fn render_pool(&self, out: &mut String) {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        c::WORKER_POOL_EMPTY.render_value(out, load(&self.empty));
        c::DISCOVERY_NO_CLIENT.render_value(out, load(&self.no_client));
        c::DISCOVERY_WATCH_BROKEN_SINCE.render_value(out, load(&self.watch_broken_since_ms) / 1000);
    }

    /// The worker-health and worker-acceptance families.
    pub(super) fn render_health(&self, out: &mut String) {
        c::WORKER_HEALTH.render(out, |series| self.health[series.at(0)].load(Ordering::Relaxed));
        c::WORKER_ACCEPTANCE
            .render(out, |series| self.acceptance[series.at(0)].load(Ordering::Relaxed));
    }
}
