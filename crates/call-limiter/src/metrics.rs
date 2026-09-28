//! [`LimiterMetrics`] — the limiter's request counters, rendered as
//! Prometheus text with the store's own events and gauges
//! ([`StoreStats`](crate::store::StoreStats)).
//!
//! Every series is `limiter_*`: counters end in `_total` and carry one closed
//! label (`outcome`), the gauges have none. No per-id labels (zero
//! cardinality risk). Metrics are GET-only and never read back into the
//! decision path.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::{AdmitResult, RefreshResult, StoreStats};

/// Clone-cheap (one `Arc` of atomics) handle to the limiter's request counters.
#[derive(Debug, Default, Clone)]
pub struct LimiterMetrics {
    inner: std::sync::Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Admits by outcome: admitted, rejected, released.
    admits: [AtomicU64; 3],
    refresh_requests: AtomicU64,
    /// Calls the refresh requests named, by outcome: extended,
    /// reregistered, released, dropped.
    refresh_calls: [AtomicU64; 4],
    release_requests: AtomicU64,
    release_calls: AtomicU64,
}

const ADMIT_OUTCOMES: [&str; 3] = ["admitted", "rejected", "released"];
const REFRESH_OUTCOMES: [&str; 4] = ["extended", "reregistered", "released", "dropped"];

fn add(a: &AtomicU64, n: u64) {
    a.fetch_add(n, Ordering::Relaxed);
}

fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

impl LimiterMetrics {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// One admit request was answered `outcome`.
    pub fn on_admit(&self, outcome: &AdmitResult) {
        let slot = match outcome {
            AdmitResult::Admitted => 0,
            AdmitResult::Rejected { .. } => 1,
            AdmitResult::Released => 2,
        };
        add(&self.inner.admits[slot], 1);
    }

    /// One refresh request was answered with `outcomes`, one per call named.
    pub fn on_refresh(&self, outcomes: &[RefreshResult]) {
        add(&self.inner.refresh_requests, 1);
        for outcome in outcomes {
            let slot = match outcome {
                RefreshResult::Extended => 0,
                RefreshResult::Reregistered => 1,
                RefreshResult::Released => 2,
                RefreshResult::Dropped => 3,
            };
            add(&self.inner.refresh_calls[slot], 1);
        }
    }

    /// One release request named `calls` calls.
    pub fn on_release(&self, calls: usize) {
        add(&self.inner.release_requests, 1);
        add(&self.inner.release_calls, calls as u64);
    }

    /// Render the full Prometheus exposition: the request counters here and
    /// the store's own counters and gauges.
    pub fn prometheus_text(&self, stats: StoreStats) -> String {
        let mut s = String::with_capacity(2048);
        let head = |s: &mut String, name: &str, kind: &str, help: &str| {
            let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        let one = |s: &mut String, name: &str, kind: &str, help: &str, v: String| {
            head(s, name, kind, help);
            let _ = writeln!(s, "{name} {v}");
        };
        head(&mut s, "limiter_admits_total", "counter", "admit requests by outcome (admitted: the whole set replaced the call's; rejected: an id the call adds is at its cap; released: the call's key is fenced by a release)");
        for (slot, outcome) in ADMIT_OUTCOMES.iter().enumerate() {
            let _ = writeln!(
                s,
                "limiter_admits_total{{outcome=\"{outcome}\"}} {}",
                get(&self.inner.admits[slot])
            );
        }
        one(
            &mut s,
            "limiter_refresh_requests_total",
            "counter",
            "refresh requests received",
            get(&self.inner.refresh_requests).to_string(),
        );
        head(&mut s, "limiter_refresh_calls_total", "counter", "calls the refresh requests named, by answer (extended; reregistered: a set the store no longer held re-created, with no cap check; released: refused by a release fence; dropped: refused because an admit of the key dropped its set)");
        for (slot, outcome) in REFRESH_OUTCOMES.iter().enumerate() {
            let _ = writeln!(
                s,
                "limiter_refresh_calls_total{{outcome=\"{outcome}\"}} {}",
                get(&self.inner.refresh_calls[slot])
            );
        }
        one(
            &mut s,
            "limiter_release_requests_total",
            "counter",
            "release requests received",
            get(&self.inner.release_requests).to_string(),
        );
        one(
            &mut s,
            "limiter_release_calls_total",
            "counter",
            "calls the release requests named, a call named again included",
            get(&self.inner.release_calls).to_string(),
        );
        one(
            &mut s,
            "limiter_lease_expired_calls_total",
            "counter",
            "call sets dropped because their lease lapsed",
            stats.lease_expired_calls.to_string(),
        );
        one(
            &mut s,
            "limiter_lease_expired_holds_total",
            "counter",
            "holds the lapsed sets carried: each one a count no release freed",
            stats.lease_expired_holds.to_string(),
        );
        one(&mut s, "limiter_calls", "gauge", "calls holding a set", stats.calls.to_string());
        one(
            &mut s,
            "limiter_holds",
            "gauge",
            "live holds over every id (the sum of every live count)",
            stats.current_total.to_string(),
        );
        one(
            &mut s,
            "limiter_fences",
            "gauge",
            "keys fenced against refresh: released calls, and calls whose set an admit dropped",
            stats.fences.to_string(),
        );
        one(
            &mut s,
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
        metrics.on_admit(&AdmitResult::Admitted);
        metrics.on_refresh(&[RefreshResult::Extended, RefreshResult::Dropped]);
        metrics.on_release(3);
        let text = metrics.prometheus_text(stats);
        for line in [
            "limiter_admits_total{outcome=\"admitted\"} 1",
            "limiter_admits_total{outcome=\"rejected\"} 0",
            "limiter_admits_total{outcome=\"released\"} 1",
            "limiter_refresh_requests_total 1",
            "limiter_refresh_calls_total{outcome=\"extended\"} 1",
            "limiter_refresh_calls_total{outcome=\"dropped\"} 1",
            "limiter_refresh_calls_total{outcome=\"reregistered\"} 0",
            "limiter_release_requests_total 1",
            "limiter_release_calls_total 3",
            "limiter_holds 7",
            "# TYPE limiter_admission_max gauge\nlimiter_admission_max 4",
            "limiter_calls 3",
            "limiter_fences 1",
            "limiter_lease_expired_calls_total 2",
            "limiter_lease_expired_holds_total 4",
        ] {
            assert!(text.contains(&format!("\n{line}\n")), "{line} missing in\n{text}");
        }
    }
}
