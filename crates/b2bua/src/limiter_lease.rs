//! [`LimiterLease`] — the limiter's lease as the worker last learnt it.
//!
//! Every admit and refresh answer states the limiter's lease; the limiter
//! client reports each one here ([`CallLimiter::report_lease`]), and the
//! worker's release queue and refresh batch give an entry up once it has
//! waited the lease read at that instant. Before any answer the lease is
//! [`DEFAULT_LEASE`], the limiter's own default. A learnt lease is clamped to
//! [`MAX_LEASE`].
//!
//! A counted call's refresh leaves up to one refresh tick after it falls due,
//! so the lease must outlast the refresh period plus a tick, or the call's
//! set lapses between two refreshes and is re-registered at each. Each change
//! of the learnt lease is checked against that reach: one the reach meets is
//! warned about and counted (`b2bua_limiter_lease_too_short_total`) once, and
//! a lease stated again changes nothing.
//!
//! [`CallLimiter::report_lease`]: crate::limiter::CallLimiter::report_lease

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::B2buaConfig;
use crate::metrics::B2buaMetrics;

/// The lease before any limiter answer: the limiter's default.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(call_limiter::DEFAULT_LEASE_SEC as u64);

/// The longest lease the worker honours: the limiter's own bound.
pub const MAX_LEASE: Duration = Duration::from_secs(call_limiter::MAX_LEASE_SEC as u64);

/// The limiter's lease as the worker last learnt it. See the module doc.
pub struct LimiterLease {
    /// The current lease, milliseconds.
    lease_ms: AtomicU64,
    /// The refresh period plus one refresh tick: a lease must outlast it.
    refresh_reach: Duration,
    metrics: B2buaMetrics,
}

impl LimiterLease {
    /// A lease starting at [`DEFAULT_LEASE`], checked against `config`'s
    /// refresh period plus its refresh tick.
    pub fn from_config(config: &B2buaConfig, metrics: B2buaMetrics) -> Arc<Self> {
        let refresh = Duration::from_secs(config.limiter_refresh_sec.max(0) as u64);
        let tick = Duration::from_millis(config.limiter_refresh_batch_ms);
        Self::new(DEFAULT_LEASE, refresh + tick, metrics)
    }

    /// A lease starting at `lease` (clamped to [`MAX_LEASE`]), checked
    /// against `refresh_reach`.
    pub fn new(lease: Duration, refresh_reach: Duration, metrics: B2buaMetrics) -> Arc<Self> {
        let lease = lease.min(MAX_LEASE);
        metrics.set_limiter_lease(lease);
        Arc::new(Self { lease_ms: AtomicU64::new(millis(lease)), refresh_reach, metrics })
    }

    /// A lease starting at `lease`, checked against nothing, on metrics of
    /// its own: for a queue or a batch built without a worker.
    pub fn starting_at(lease: Duration) -> Arc<Self> {
        Self::new(lease, Duration::ZERO, B2buaMetrics::new())
    }

    /// The lease as last learnt.
    pub fn current(&self) -> Duration {
        Duration::from_millis(self.lease_ms.load(Ordering::Relaxed))
    }

    /// The limiter stated `lease`: it becomes the current lease (clamped to
    /// [`MAX_LEASE`]). A change the refresh reach meets is warned about and
    /// counted.
    pub fn learn(&self, lease: Duration) {
        let lease = lease.min(MAX_LEASE);
        let ms = millis(lease);
        let before = self.lease_ms.swap(ms, Ordering::Relaxed);
        if before == ms {
            return;
        }
        self.metrics.set_limiter_lease(lease);
        if lease <= self.refresh_reach {
            self.metrics.bump_limiter_lease_too_short();
            tracing::warn!(
                lease_ms = ms,
                before_ms = before,
                refresh_reach_ms = millis(self.refresh_reach),
                "the limiter's lease is not longer than the refresh period plus a refresh tick: \
                 counted calls lapse between refreshes and are re-registered at each"
            );
        } else {
            tracing::info!(lease_ms = ms, before_ms = before, "limiter lease learnt");
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(metrics: &B2buaMetrics) -> Arc<LimiterLease> {
        // Refresh 40 s + tick 1 s.
        LimiterLease::new(DEFAULT_LEASE, Duration::from_secs(41), metrics.clone())
    }

    #[test]
    fn the_default_holds_until_an_answer_states_a_lease() {
        let metrics = B2buaMetrics::new();
        let l = lease(&metrics);
        assert_eq!(l.current(), Duration::from_secs(120));
        assert_eq!(metrics.limiter_lease(), Duration::from_secs(120));
        l.learn(Duration::from_secs(60));
        assert_eq!(l.current(), Duration::from_secs(60));
        assert_eq!(metrics.limiter_lease(), Duration::from_secs(60));
        assert_eq!(metrics.limiter_lease_too_short_total(), 0, "41 s < 60 s");
    }

    #[test]
    fn a_lease_the_refresh_reaches_is_counted_once_per_change() {
        let metrics = B2buaMetrics::new();
        let l = lease(&metrics);
        l.learn(Duration::from_secs(41));
        l.learn(Duration::from_secs(41));
        assert_eq!(metrics.limiter_lease_too_short_total(), 1, "the same lease counts once");
        l.learn(Duration::from_secs(30));
        assert_eq!(metrics.limiter_lease_too_short_total(), 2, "a new short lease counts");
        l.learn(Duration::from_secs(120));
        l.learn(Duration::from_secs(30));
        assert_eq!(metrics.limiter_lease_too_short_total(), 3, "back to short counts again");
    }

    #[test]
    fn a_learnt_lease_is_clamped_to_one_day() {
        let l = LimiterLease::starting_at(Duration::from_secs(20));
        l.learn(Duration::from_secs(10 * 365 * 24 * 3600));
        assert_eq!(l.current(), MAX_LEASE);
        let l = LimiterLease::starting_at(Duration::MAX);
        assert_eq!(l.current(), MAX_LEASE);
    }
}
