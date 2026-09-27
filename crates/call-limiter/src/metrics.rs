//! [`LimiterMetrics`] — the request counters the store does not keep, rendered
//! as Prometheus text with the [`StoreStats`](crate::store::StoreStats) gauges
//! and counters.
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
    refresh: AtomicU64,
}

impl LimiterMetrics {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// One admit request arrived, with its outcome (a refusal of a released
    /// call is counted by the store).
    pub fn on_admit(&self, outcome: &AdmitResult) {
        self.inner.admit.fetch_add(1, Ordering::Relaxed);
        match outcome {
            AdmitResult::Admitted => self.inner.admitted.fetch_add(1, Ordering::Relaxed),
            AdmitResult::Rejected { .. } => self.inner.rejected.fetch_add(1, Ordering::Relaxed),
            AdmitResult::Released => 0,
        };
    }

    /// One refresh request arrived.
    pub fn on_refresh(&self) {
        self.inner.refresh.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the full Prometheus exposition: the request counters here and
    /// the store's own counters and gauges.
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
            stats.admits_refused_released.to_string(),
        );
        metric(
            "limiter_release_total",
            "counter",
            "keys named by the release requests received, a key named again included",
            stats.releases_total.to_string(),
        );
        metric(
            "limiter_refresh_total",
            "counter",
            "refresh requests received",
            g(&self.inner.refresh).to_string(),
        );
        metric(
            "limiter_reregistered_calls_total",
            "counter",
            "call sets re-created by a refresh of a call the store no longer held",
            stats.reregistered_calls.to_string(),
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
            "limiter_fences",
            "gauge",
            "keys fenced against refresh: released calls, and calls whose set an admit dropped",
            stats.fences.to_string(),
        );
        metric(
            "limiter_current_total",
            "gauge",
            "sum of all live counts (current concurrent across ids)",
            stats.current_total.to_string(),
        );
        metric(
            "limiter_admission_max",
            "gauge",
            "largest live count of one id (what an admit of that id compares with its cap)",
            stats.admission_max.to_string(),
        );
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_store_counters_beside_the_request_counters() {
        let stats = StoreStats {
            calls: 3,
            current_total: 7,
            admission_max: 4,
            fences: 1,
            lease_expired_calls: 2,
            lease_expired_holds: 4,
            reregistered_calls: 1,
            releases_total: 5,
            admits_refused_released: 6,
        };
        let metrics = LimiterMetrics::new();
        metrics.on_admit(&AdmitResult::Released);
        let text = metrics.prometheus_text(stats);
        assert!(text.contains("\nlimiter_admit_total 1\n"), "{text}");
        assert!(text.contains("\nlimiter_current_total 7\n"), "{text}");
        assert!(
            text.contains("\n# TYPE limiter_admission_max gauge\nlimiter_admission_max 4\n"),
            "{text}"
        );
        assert!(text.contains("\nlimiter_calls 3\n"), "{text}");
        assert!(text.contains("\nlimiter_fences 1\n"), "{text}");
        assert!(text.contains("\nlimiter_lease_expired_calls_total 2\n"), "{text}");
        assert!(text.contains("\nlimiter_lease_expired_holds_total 4\n"), "{text}");
        assert!(text.contains("\nlimiter_reregistered_calls_total 1\n"), "{text}");
        assert!(text.contains("\nlimiter_release_total 5\n"), "{text}");
        assert!(text.contains("\nlimiter_admit_released_total 6\n"), "{text}");
    }
}
