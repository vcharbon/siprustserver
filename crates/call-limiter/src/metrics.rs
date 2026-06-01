//! [`LimiterMetrics`] — global request counters, rendered as Prometheus text
//! alongside the [`WindowStats`](crate::window::WindowStats) gauges.
//!
//! No per-id labels (matches the TS global counters; zero cardinality risk).
//! Metrics are GET-only and never read back into the decision path, so this
//! surface is freely refactorable later.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::window::WindowStats;

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
    release: AtomicU64,
    refresh: AtomicU64,
}

impl LimiterMetrics {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// One admit request arrived (incremented on every `/v1/admit`).
    pub fn on_admit(&self, admitted: bool) {
        self.inner.admit.fetch_add(1, Ordering::Relaxed);
        if admitted {
            self.inner.admitted.fetch_add(1, Ordering::Relaxed);
        } else {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One release request arrived.
    pub fn on_release(&self) {
        self.inner.release.fetch_add(1, Ordering::Relaxed);
    }

    /// One refresh request arrived.
    pub fn on_refresh(&self) {
        self.inner.refresh.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the full Prometheus exposition, combining request counters with
    /// the live store gauges + the auto-clear counter.
    pub fn prometheus_text(&self, stats: WindowStats) -> String {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut s = String::with_capacity(1024);
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
            "admits where all entries were admitted",
            g(&self.inner.admitted).to_string(),
        );
        metric(
            "limiter_rejected_total",
            "counter",
            "admits rejected (an entry over cap)",
            g(&self.inner.rejected).to_string(),
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
            "limiter_auto_cleared_total",
            "counter",
            "window keys removed by TTL sweep",
            stats.auto_cleared.to_string(),
        );
        metric(
            "limiter_live_keys",
            "gauge",
            "live (id,window) keys currently held",
            stats.live_keys.to_string(),
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
