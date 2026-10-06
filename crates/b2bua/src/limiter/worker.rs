//! [`LimiterWorker`] — the worker's one handle on its call limiter.
//!
//! [`LimiterWorker::start`] assembles the worker side over the limiter
//! client: the learnt [`LimiterLease`] the client reports to, the
//! [`ReleaseQueue`] and the [`RefreshBatch`] (both sending through the client
//! itself; a key whose release is queued is forgotten by the batch, and each
//! refresh answer comes back to its call as a re-entrant event), and the
//! admit path `Breaker(Bounded(client))`, or `Bounded(client)` for a client
//! without a health answer ([`crate::limiter::bounded`],
//! [`crate::limiter::breaker`]). The handle is a cheap clone; every call's
//! admit, release and refresh, and every exit's last release send, go
//! through it.

use std::sync::Arc;
use std::time::Duration;

use call::{AdmitReport, LimiterEntry, LimiterHeld};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::B2buaConfig;
use crate::limiter::bounded::BoundedLimiter;
use crate::limiter::breaker::{BreakerConfig, BreakerLimiter};
use crate::limiter::lease::LimiterLease;
use crate::limiter::refresh_batch::{RefreshBatch, RefreshBatchConfig};
use crate::limiter::release_queue::{ReleaseFlush, ReleaseQueue, ReleaseQueueConfig};
use crate::limiter::{AdmitOutcome, CallLimiter, LimiterReports};
use crate::metrics::{AdmitSite, B2buaMetrics};
use b2bua_sdk::event::CallEvent;

/// The worker's handle on its call limiter. See the module doc.
#[derive(Clone)]
pub struct LimiterWorker(Arc<Parts>);

struct Parts {
    /// The admit path: the client behind its bound, and its breaker.
    admits: Arc<dyn CallLimiter>,
    releases: Arc<ReleaseQueue>,
    refreshes: Arc<RefreshBatch>,
    lease: Arc<LimiterLease>,
    metrics: B2buaMetrics,
}

impl LimiterWorker {
    /// The worker side over `limiter` under `config`, its counts on
    /// `metrics`, each refresh answer posted on `reentry`; and the handles of
    /// the tasks it runs (the release queue's sender, the refresh batch's
    /// sender, the breaker's probe when guarded). Dropping every clone of the
    /// handle leaves them running: the caller owns the returned handles and
    /// aborts them with the node's other tasks.
    pub(crate) fn start(
        limiter: Arc<dyn CallLimiter>,
        config: &B2buaConfig,
        metrics: B2buaMetrics,
        reentry: mpsc::UnboundedSender<CallEvent>,
    ) -> (Self, Vec<JoinHandle<()>>) {
        let lease = LimiterLease::from_config(config, metrics.clone());
        limiter.report_to(LimiterReports::new(&lease, metrics.clone()));
        let releases = ReleaseQueue::new(
            limiter.clone(),
            ReleaseQueueConfig::from_config(config, lease.clone()),
            metrics.clone(),
        );
        let mut tasks = vec![tokio::spawn(releases.clone().run())];
        let refreshes = RefreshBatch::new(
            limiter.clone(),
            RefreshBatchConfig::from_config(config, lease.clone()),
            metrics.clone(),
            move |answer| {
                let _ = reentry.send(answer.into_event());
            },
        );
        let forget = refreshes.clone();
        let hooked = releases.on_push(move |key| forget.forget(key));
        debug_assert!(hooked, "one refresh batch per release queue");
        tasks.push(tokio::spawn(refreshes.clone().run()));
        let (admits, breaker) = BreakerLimiter::guard(
            BoundedLimiter::wrap(limiter, metrics.clone()),
            BreakerConfig::from_config(config),
            releases.clone(),
            refreshes.clone(),
            metrics.clone(),
        );
        if let Some(breaker) = breaker {
            tasks.push(tokio::spawn(breaker.run()));
        }
        let worker = Self(Arc::new(Parts { admits, releases, refreshes, lease, metrics }));
        (worker, tasks)
    }

    /// Send one admit of `entries` for `key` under the call's `change`
    /// number, carrying the call's `held` set (see [`CallLimiter::admit`]):
    /// the report the call applies, answered within the admit budget plus
    /// [`ADMIT_SLACK`](crate::limiter::bounded::ADMIT_SLACK). An admit a
    /// release fence refused is counted under `site`.
    pub(crate) async fn admit(
        &self,
        site: AdmitSite,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: Vec<LimiterEntry>,
        release_on_refusal: bool,
    ) -> AdmitReport {
        let outcome = self.0.admits.admit(key, change, held, &entries, release_on_refusal).await;
        if outcome == AdmitOutcome::Released {
            self.0.metrics.limiter().count_admit_released(site);
        }
        AdmitReport { key: key.to_string(), change, entries, outcome }
    }

    /// Queue the release of `key`'s set; no call waits on it.
    pub fn release(&self, key: &str) {
        self.0.releases.push(key);
    }

    /// Queue the release `report` owes ([`AdmitReport::owed_release`]) for an
    /// admit no call takes: its call is gone, or the router closed before
    /// the report reached it. The call's own terminal release may have run
    /// already: the server applies this one as a no-op then.
    pub(crate) fn release_unclaimed(&self, report: &AdmitReport) {
        if let Some(key) = report.owed_release() {
            self.release(key);
        }
    }

    /// Mark `key`'s refresh due for `call_ref`, carrying the set it `held`;
    /// the answer comes back to the call as a re-entrant event.
    pub(crate) fn refresh_due(&self, key: &str, call_ref: &str, held: &LimiterHeld) {
        self.0.refreshes.mark(key, call_ref, held);
    }

    /// The refresh period the learnt lease sets.
    pub(crate) fn refresh_period(&self) -> Duration {
        self.0.lease.refresh_period()
    }

    /// The limiter's admit budget.
    pub(crate) fn admit_budget(&self) -> Duration {
        self.0.admits.admit_budget()
    }

    /// A planned exit's last release send (`ReleaseQueue::flush`).
    pub async fn flush(&self, within: Duration) -> ReleaseFlush {
        self.0.releases.flush(within).await
    }

    /// Give up every queued release, counted and logged; how many
    /// (`ReleaseQueue::give_up_all`).
    pub fn give_up_all(&self) -> usize {
        self.0.releases.give_up_all()
    }

    /// The worker crashed: its queued releases are lost as they stand, and
    /// a flush returns at once.
    pub(crate) fn stop(&self) {
        self.0.releases.stop();
    }

    /// Whether the worker crashed (its release queue stopped).
    pub fn is_stopped(&self) -> bool {
        self.0.releases.is_stopped()
    }

    /// Releases queued and not yet answered, the one in flight included.
    pub fn waiting(&self) -> usize {
        self.0.releases.waiting()
    }

    /// The keys whose release is queued, oldest first.
    pub fn waiting_keys(&self) -> Vec<String> {
        self.0.releases.waiting_keys()
    }

    /// Releases a flush still has to send: none once stopped.
    pub(crate) fn unsent(&self) -> usize {
        self.0.releases.unsent()
    }

    /// A release request is in flight.
    pub fn sending(&self) -> bool {
        self.0.releases.sending()
    }

    /// Hold the release queue, as an open breaker does: what is queued stays
    /// readable.
    #[cfg(test)]
    pub(crate) fn hold_releases(&self) {
        self.0.releases.hold();
    }

    /// Calls whose refresh is marked due.
    #[cfg(test)]
    pub(crate) fn refreshes_due(&self) -> usize {
        self.0.refreshes.due()
    }
}
