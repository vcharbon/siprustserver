//! The load balancer's per-worker overload path: new dialogs refused by
//! reason, spilled past a rate-capped rendezvous winner, emergency ones that
//! bypassed it, and the stale-payload floor events of a worker's AIMD cap.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_values::FixedCounts;

use super::super::catalogue as c;
use super::ProxyMetrics;

pub(super) struct OverloadCounts {
    rejections: FixedCounts,
    /// Coarse (registry-aggregate) count of `WorkerLoadObserver::sweep_stale`
    /// floor events: an Alive worker whose OPTIONS answers stopped carrying a
    /// fresh `X-Overload` within `payload_stale_ms`, so AIMD halved its cap.
    /// Non-zero while ELU is healthy means the probe cycle exceeds the stale
    /// threshold (config invariant).
    stale_decrease: AtomicU64,
    /// Emergency new-dialog INVITEs that skipped the `above_critical` filter
    /// and the per-worker AIMD bucket.
    emergency_bypassed: AtomicU64,
    /// New dialogs placed past a rate-capped rendezvous winner (ADR-0009 X6).
    rate_cap_spilled: AtomicU64,
}

impl Default for OverloadCounts {
    fn default() -> Self {
        Self {
            rejections: FixedCounts::new(&c::OVERLOAD_REJECTIONS),
            stale_decrease: AtomicU64::default(),
            emergency_bypassed: AtomicU64::default(),
            rate_cap_spilled: AtomicU64::default(),
        }
    }
}

impl OverloadCounts {
    pub(super) fn render(&self, s: &mut String) {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        self.rejections.render(s);
        c::WORKER_STALE_DECREASE.render_value(s, load(&self.stale_decrease));
        c::LB_EMERGENCY_BYPASSED.render_value(s, load(&self.emergency_bypassed));
        c::LB_RATE_CAP_SPILLED.render_value(s, load(&self.rate_cap_spilled));
    }
}

impl ProxyMetrics {
    /// One new dialog the overload path refused, for
    /// `sip_proxy_overload_rejections_total{reason}`.
    pub fn record_overload_rejection(&self, reason: &str) {
        self.overload.rejections.add(&[reason], 1);
    }

    /// Add `n` `sweep_stale` floor events (one per worker the sweep just
    /// conservatively decreased). The runner's 1 s sweep task calls this with
    /// the per-sweep floored count.
    pub fn record_overload_stale_decrease(&self, n: u64) {
        self.overload.stale_decrease.fetch_add(n, Ordering::Relaxed);
    }

    /// One emergency new-dialog INVITE that bypassed the LB's overload gates
    /// (critical-filter + AIMD bucket), for
    /// `sip_proxy_lb_emergency_bypassed_total`.
    pub fn record_lb_emergency_bypass(&self) {
        self.overload.emergency_bypassed.fetch_add(1, Ordering::Relaxed);
    }

    /// One new dialog placed on a later rendezvous candidate because the
    /// winner's bucket was empty, for `sip_proxy_lb_rate_cap_spilled_total`.
    pub fn record_lb_rate_cap_spill(&self) {
        self.overload.rate_cap_spilled.fetch_add(1, Ordering::Relaxed);
    }

    pub fn overload_rejection_count(&self, reason: &str) -> u64 {
        self.overload.rejections.get(&[reason])
    }
    pub fn overload_stale_decrease_total(&self) -> u64 {
        self.overload.stale_decrease.load(Ordering::Relaxed)
    }
    pub fn lb_emergency_bypassed_total(&self) -> u64 {
        self.overload.emergency_bypassed.load(Ordering::Relaxed)
    }
    pub fn lb_rate_cap_spilled_total(&self) -> u64 {
        self.overload.rate_cap_spilled.load(Ordering::Relaxed)
    }
}
