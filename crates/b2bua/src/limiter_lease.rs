//! [`LimiterLease`] — the limiter's lease as the worker last learnt it, and
//! the refresh period it sets.
//!
//! Every admit, refresh and health answer states the limiter's lease; the
//! limiter client reports each one here ([`LimiterReports`]), and the worker's
//! release queue and refresh batch give an entry up once it has waited the
//! lease read at that instant; each of them watches [`LimiterLease::changes`]
//! so a shorter lease learnt while it waits takes effect at the new deadline.
//! Before any answer the lease is [`DEFAULT_LEASE`], the limiter's own
//! default. A learnt lease is clamped to [`MAX_LEASE`].
//!
//! A counted call refreshes every [`refresh_period`](LimiterLease::refresh_period):
//! the configured period, or a third of the lease when that is shorter, so two
//! refreshes may be missed before a set lapses. A refresh leaves up to one
//! tick after it falls due, so the configured period plus a tick must stay
//! below the lease. The first lease stated, and each change after it, is
//! checked: one the configured period plus a tick reaches is warned about and
//! counted (`b2bua_limiter_lease_too_short_total`), and one that shortens the
//! period is counted (`b2bua_limiter_refresh_period_clamped_total`). A lease
//! stated again changes nothing.
//!
//! [`LimiterReports`]: crate::limiter::LimiterReports

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::config::B2buaConfig;
use crate::metrics::B2buaMetrics;

/// The lease before any limiter answer: the limiter's default.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(call_limiter::DEFAULT_LEASE_SEC as u64);

/// The longest lease the worker honours: the limiter's own bound.
pub const MAX_LEASE: Duration = Duration::from_secs(call_limiter::MAX_LEASE_SEC as u64);

/// How many refresh periods fit in the lease at the least.
const PERIODS_PER_LEASE: u32 = 3;

/// The worker's refresh pace, checked against every learnt lease.
#[derive(Clone, Copy, Debug)]
struct RefreshPace {
    /// The configured refresh period.
    period: Duration,
    /// The refresh batch's tick: a refresh due leaves within it.
    tick: Duration,
}

/// The limiter's lease as the worker last learnt it. See the module doc.
pub struct LimiterLease {
    /// The current lease; its receivers wake on each change.
    lease: watch::Sender<Duration>,
    /// Whether an answer stated a lease yet.
    learnt: AtomicBool,
    /// `None` for a lease no worker refreshes under: nothing is checked.
    pace: Option<RefreshPace>,
    metrics: B2buaMetrics,
}

impl LimiterLease {
    /// A lease starting at [`DEFAULT_LEASE`], under `config`'s refresh period
    /// and refresh tick.
    pub fn from_config(config: &B2buaConfig, metrics: B2buaMetrics) -> Arc<Self> {
        let pace = RefreshPace {
            period: Duration::from_secs(config.limiter_refresh_sec.max(1) as u64),
            tick: Duration::from_millis(config.limiter_refresh_batch_ms),
        };
        Self::build(DEFAULT_LEASE, Some(pace), metrics)
    }

    /// A lease starting at `lease`, under no refresh pace, on metrics of its
    /// own: for a queue or a batch built without a worker.
    pub fn starting_at(lease: Duration) -> Arc<Self> {
        Self::build(lease, None, B2buaMetrics::new())
    }

    fn build(lease: Duration, pace: Option<RefreshPace>, metrics: B2buaMetrics) -> Arc<Self> {
        let lease = lease.min(MAX_LEASE);
        let this = Self {
            lease: watch::Sender::new(lease),
            learnt: AtomicBool::new(false),
            pace,
            metrics,
        };
        this.publish(lease);
        Arc::new(this)
    }

    /// The lease as last learnt.
    pub fn current(&self) -> Duration {
        *self.lease.borrow()
    }

    /// A receiver that wakes on each change of the lease.
    pub fn changes(&self) -> watch::Receiver<Duration> {
        self.lease.subscribe()
    }

    /// How often a counted call refreshes under the current lease: the
    /// configured period, or a third of the lease when shorter.
    pub fn refresh_period(&self) -> Duration {
        refresh_period(self.pace, self.current())
    }

    /// The limiter stated `lease`: it becomes the current lease (clamped to
    /// [`MAX_LEASE`]). The first lease stated and each change are checked.
    pub fn learn(&self, lease: Duration) {
        let lease = lease.min(MAX_LEASE);
        let first = !self.learnt.swap(true, Ordering::AcqRel);
        let changed = self.lease.send_if_modified(|current| {
            if *current == lease {
                return false;
            }
            *current = lease;
            self.publish(lease);
            true
        });
        if first || changed {
            self.check(lease);
        }
    }

    /// Publish the gauges of `lease`, under the watch's lock so the gauges
    /// read the value stored.
    fn publish(&self, lease: Duration) {
        self.metrics.limiter().set_lease(lease);
        self.metrics.limiter().set_refresh_period(refresh_period(self.pace, lease));
    }

    /// Warn about and count a learnt `lease` the pace does not fit.
    fn check(&self, lease: Duration) {
        let Some(pace) = self.pace else {
            return;
        };
        let lease_ms = millis(lease);
        let period_ms = millis(refresh_period(Some(pace), lease));
        if pace.period + pace.tick >= lease {
            self.metrics.limiter().count_lease_too_short();
            tracing::warn!(
                lease_ms,
                configured_refresh_ms = millis(pace.period),
                tick_ms = millis(pace.tick),
                refresh_ms = period_ms,
                "the limiter's lease is not longer than the configured refresh period plus a \
                 refresh tick; counted calls refresh every third of the lease instead"
            );
        } else {
            tracing::info!(lease_ms, refresh_ms = period_ms, "limiter lease learnt");
        }
        if lease / PERIODS_PER_LEASE < pace.period {
            self.metrics.limiter().count_refresh_period_clamped();
        }
    }
}

/// The refresh period under `pace` and `lease`.
fn refresh_period(pace: Option<RefreshPace>, lease: Duration) -> Duration {
    let third = lease / PERIODS_PER_LEASE;
    pace.map_or(third, |pace| pace.period.min(third))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker's lease under a 40 s refresh period and a 1 s tick.
    fn lease(metrics: &B2buaMetrics) -> Arc<LimiterLease> {
        LimiterLease::from_config(&B2buaConfig::default(), metrics.clone())
    }

    #[test]
    fn the_default_holds_until_an_answer_states_a_lease() {
        let metrics = B2buaMetrics::new();
        let l = lease(&metrics);
        assert_eq!(l.current(), Duration::from_secs(120));
        assert_eq!(metrics.limiter().lease(), Duration::from_secs(120));
        assert_eq!(l.refresh_period(), Duration::from_secs(40));
        l.learn(Duration::from_secs(150));
        assert_eq!(l.current(), Duration::from_secs(150));
        assert_eq!(metrics.limiter().lease(), Duration::from_secs(150));
        assert_eq!(metrics.limiter().lease_too_short_total(), 0, "41 s < 150 s");
        assert_eq!(metrics.limiter().refresh_period_clamped_total(), 0, "40 s <= 50 s");
    }

    #[test]
    fn the_first_stated_lease_is_checked_even_when_it_equals_the_default() {
        let metrics = B2buaMetrics::new();
        let config = B2buaConfig { limiter_refresh_sec: 120, ..Default::default() };
        let l = LimiterLease::from_config(&config, metrics.clone());
        assert_eq!(metrics.limiter().lease_too_short_total(), 0, "the default is not checked");
        l.learn(DEFAULT_LEASE);
        assert_eq!(metrics.limiter().lease_too_short_total(), 1);
        assert_eq!(metrics.limiter().refresh_period_clamped_total(), 1);
        assert_eq!(l.refresh_period(), Duration::from_secs(40), "a third of the lease");
        l.learn(DEFAULT_LEASE);
        assert_eq!(metrics.limiter().lease_too_short_total(), 1, "stated again: nothing");
    }

    #[test]
    fn a_lease_the_refresh_reaches_is_counted_once_per_change() {
        let metrics = B2buaMetrics::new();
        let l = lease(&metrics);
        l.learn(Duration::from_secs(41));
        l.learn(Duration::from_secs(41));
        assert_eq!(metrics.limiter().lease_too_short_total(), 1, "the same lease counts once");
        l.learn(Duration::from_secs(30));
        assert_eq!(metrics.limiter().lease_too_short_total(), 2, "a new short lease counts");
        l.learn(Duration::from_secs(120));
        l.learn(Duration::from_secs(30));
        assert_eq!(metrics.limiter().lease_too_short_total(), 3, "back to short counts again");
        assert_eq!(metrics.limiter().refresh_period(), Duration::from_secs(10));
    }

    #[test]
    fn a_learnt_lease_is_clamped_to_one_day() {
        let l = LimiterLease::starting_at(Duration::from_secs(20));
        l.learn(Duration::from_secs(10 * 365 * 24 * 3600));
        assert_eq!(l.current(), MAX_LEASE);
        let l = LimiterLease::starting_at(Duration::MAX);
        assert_eq!(l.current(), MAX_LEASE);
    }

    #[tokio::test]
    async fn a_change_wakes_its_watchers_and_a_repeat_does_not() {
        let l = LimiterLease::starting_at(Duration::from_secs(20));
        let mut changes = l.changes();
        l.learn(Duration::from_secs(20));
        assert!(!changes.has_changed().unwrap(), "stated again");
        l.learn(Duration::from_secs(5));
        assert!(changes.has_changed().unwrap());
        assert_eq!(*changes.borrow_and_update(), Duration::from_secs(5));
    }
}
