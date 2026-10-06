//! [`LimiterMetrics`] — the limiter's request counters, rendered as
//! Prometheus text with the store's own events and gauges
//! ([`StoreStats`](crate::store::StoreStats)).
//!
//! Every series is `limiter_*`: counters end in `_total` and carry one closed
//! label (`outcome`), the gauges have none. No per-id labels (zero
//! cardinality risk). Metrics are GET-only and never read back into the
//! decision path.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::{AdmitResult, RefreshResult, StoreStats};

/// Clone-cheap (one `Arc` of atomics) handle to the limiter's request counters.
#[derive(Debug, Default, Clone)]
pub struct LimiterMetrics {
    inner: std::sync::Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Admits by outcome: admitted, rejected, released, superseded.
    admits: [AtomicU64; 4],
    refresh_requests: AtomicU64,
    /// Calls the refresh requests named, by outcome: extended,
    /// reregistered, released, dropped.
    refresh_calls: [AtomicU64; 4],
    release_requests: AtomicU64,
    release_calls: AtomicU64,
}

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
            AdmitResult::Superseded { .. } => 3,
        };
        add(&self.inner.admits[slot], 1);
    }

    /// One refresh request was answered with `outcomes`, one per call named.
    pub fn on_refresh<'a>(&self, outcomes: impl IntoIterator<Item = &'a RefreshResult>) {
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
    /// the store's own counters and gauges, every family of
    /// [`crate::catalogue::CATALOGUE`].
    pub fn prometheus_text(&self, stats: StoreStats) -> String {
        use crate::catalogue as c;
        let mut s = String::with_capacity(2048);
        c::ADMITS.render(&mut s, |series| get(&self.inner.admits[series.at(0)]));
        c::REFRESH_REQUESTS.render_value(&mut s, get(&self.inner.refresh_requests));
        c::REFRESH_CALLS.render(&mut s, |series| get(&self.inner.refresh_calls[series.at(0)]));
        c::ADMIT_REREGISTERED_CALLS.render_value(&mut s, stats.admit_reregistered_calls);
        c::RELEASE_REQUESTS.render_value(&mut s, get(&self.inner.release_requests));
        c::RELEASE_CALLS.render_value(&mut s, get(&self.inner.release_calls));
        c::LEASE_EXPIRED_CALLS.render_value(&mut s, stats.lease_expired_calls);
        c::LEASE_EXPIRED_HOLDS.render_value(&mut s, stats.lease_expired_holds);
        c::CALLS.render_value(&mut s, stats.calls);
        c::HOLDS.render_value(&mut s, stats.current_total);
        c::FENCES.render_value(&mut s, stats.fences);
        c::CHANGE_MARKERS.render_value(&mut s, stats.change_markers);
        c::ADMISSION_MAX.render_value(&mut s, stats.admission_max);
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
            change_markers: 0,
            lease_expired_calls: 2,
            lease_expired_holds: 4,
            reregistered_calls: 1,
            admit_reregistered_calls: 8,
            releases_total: 5,
            admits_refused_released: 6,
        };
        let metrics = LimiterMetrics::new();
        metrics.on_admit(&AdmitResult::Released);
        metrics.on_admit(&AdmitResult::Admitted);
        metrics.on_refresh(&[RefreshResult::Extended, RefreshResult::Dropped]);
        metrics.on_release(3);
        let text = metrics.prometheus_text(stats);
        assert_eq!(crate::CATALOGUE.check(&text), Ok(()));
        for line in [
            "limiter_admits_total{outcome=\"admitted\"} 1",
            "limiter_admits_total{outcome=\"rejected\"} 0",
            "limiter_admits_total{outcome=\"released\"} 1",
            "limiter_admits_total{outcome=\"superseded\"} 0",
            "limiter_refresh_requests_total 1",
            "limiter_refresh_calls_total{outcome=\"extended\"} 1",
            "limiter_refresh_calls_total{outcome=\"dropped\"} 1",
            "limiter_refresh_calls_total{outcome=\"reregistered\"} 0",
            "limiter_admit_reregistered_calls_total 8",
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
