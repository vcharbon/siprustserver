//! [`LimiterMetrics`] — global request counters, rendered as Prometheus text
//! alongside the [`StoreStats`](crate::store::StoreStats) gauges.
//!
//! No per-id labels (zero cardinality risk). Metrics are GET-only and never
//! read back into the decision path.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::{AdmitResult, StoreStats};

/// Clone-cheap (one `Arc` of atomics) handle to the limiter's request counters.
#[derive(Debug, Default, Clone)]
pub struct LimiterMetrics {
    inner: std::sync::Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    admit: AtomicU64,
    admitted: AtomicU64,
    rejected: AtomicU64,
    refused_released: AtomicU64,
    release: AtomicU64,
    refresh: AtomicU64,
    refresh_unknown: AtomicU64,
}

impl LimiterMetrics {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// One admit request arrived, with its outcome.
    pub fn on_admit(&self, outcome: &AdmitResult) {
        self.inner.admit.fetch_add(1, Ordering::Relaxed);
        let counter = match outcome {
            AdmitResult::Admitted => &self.inner.admitted,
            AdmitResult::Rejected { .. } => &self.inner.rejected,
            AdmitResult::Released => &self.inner.refused_released,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// One release request arrived.
    pub fn on_release(&self) {
        self.inner.release.fetch_add(1, Ordering::Relaxed);
    }

    /// One refresh request arrived; `known` is whether the call had a set.
    pub fn on_refresh(&self, known: bool) {
        self.inner.refresh.fetch_add(1, Ordering::Relaxed);
        if !known {
            self.inner.refresh_unknown.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Render the full Prometheus exposition, combining request counters with
    /// the live store gauges and the lease counters.
    pub fn prometheus_text(&self, stats: StoreStats) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut s = String::with_capacity(1536);
        let mut metric = |name: &str, kind: &str, help: &str, value: String| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"));
        };
        metric(
            "limiter_admit_total",
            "counter",
            "admit requests received",
            g(&self.inner.admit).to_string(),
        );
        metric(
            "limiter_admitted_total",
            "counter",
            "admits where the whole set was admitted",
            g(&self.inner.admitted).to_string(),
        );
        metric(
            "limiter_rejected_total",
            "counter",
            "admits refused on a cap",
            g(&self.inner.rejected).to_string(),
        );
        metric(
            "limiter_admit_released_total",
            "counter",
            "admits refused because the call was already released",
            g(&self.inner.refused_released).to_string(),
        );
        metric(
            "limiter_release_total",
            "counter",
            "release requests received",
            g(&self.inner.release).to_string(),
        );
        metric(
            "limiter_refresh_total",
            "counter",
            "refresh requests received",
            g(&self.inner.refresh).to_string(),
        );
        metric(
            "limiter_refresh_unknown_total",
            "counter",
            "refreshes of a call the store holds no set for",
            g(&self.inner.refresh_unknown).to_string(),
        );
        metric(
            "limiter_lease_expired_calls_total",
            "counter",
            "call sets dropped because their lease lapsed",
            stats.lease_expired_calls.to_string(),
        );
        metric(
            "limiter_lease_expired_holds_total",
            "counter",
            "holds those sets carried",
            stats.lease_expired_holds.to_string(),
        );
        metric("limiter_calls", "gauge", "calls holding a set", stats.calls.to_string());
        metric(
            "limiter_tombstones",
            "gauge",
            "released calls still tombstoned",
            stats.tombstones.to_string(),
        );
        metric(
            "limiter_current_total",
            "gauge",
            "sum of all live counts (current concurrent across ids)",
            stats.current_total.to_string(),
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_lease_counters_beside_the_live_total() {
        let stats = StoreStats {
            calls: 3,
            current_total: 7,
            tombstones: 1,
            lease_expired_calls: 2,
            lease_expired_holds: 4,
            releases_total: 5,
            admits_refused_released: 0,
        };
        let metrics = LimiterMetrics::new();
        metrics.on_admit(&AdmitResult::Released);
        let text = metrics.prometheus_text(stats);
        assert!(text.contains("\nlimiter_current_total 7\n"), "{text}");
        assert!(text.contains("\nlimiter_calls 3\n"), "{text}");
        assert!(text.contains("\nlimiter_tombstones 1\n"), "{text}");
        assert!(
            text.contains(
                "\n# TYPE limiter_lease_expired_calls_total counter\nlimiter_lease_expired_calls_total 2\n"
            ),
            "{text}"
        );
        assert!(text.contains("\nlimiter_lease_expired_holds_total 4\n"), "{text}");
        assert!(text.contains("\nlimiter_admit_released_total 1\n"), "{text}");
    }
}
