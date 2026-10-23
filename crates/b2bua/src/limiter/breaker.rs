//! [`BreakerLimiter`] — the worker's policy on its call limiter's circuit
//! breaker (ADR-0040, ADR-0043).
//!
//! The breaker is the limiter client's backend breaker
//! ([`crate::limiter::LimiterHealth::breaker`], `http_net::backend`): it opens when the last
//! 2 s saw at least 3 requests sent to the limiter (admits, refreshes,
//! releases) and not one HTTP response, and its probe asks the limiter's
//! health every second until any response closes it. Opening forgets the
//! limiter's address, so the probe reaches a limiter that moved.
//!
//! **Open**, an admit sends nothing and answers [`AdmitOutcome::NotSent`] at
//! once: the call owes no release by that admit, and stays as it was (an
//! initial admit leaves it uncounted, a reroute admit of a counted call
//! leaves it counted on its old set). The worker's refresh batch and release
//! queue are held: a refresh sends nothing, the batch keeps it due. Answers
//! of admits sent before the breaker opened change nothing. **Closed**
//! again, the queue and the batch resume: every waiting release and every
//! refresh due leave at once, and each refresh's answer reaches its call.
//!
//! A limiter whose address is not known ([`crate::limiter::LimiterHealth::has_address`]: its
//! name has not resolved) starts with its breaker open. A limiter without a
//! breaker runs unguarded, and a guarded limiter has none: it is never
//! guarded twice.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::watch;

use crate::limiter::refresh_batch::RefreshBatch;
use crate::limiter::release_queue::ReleaseQueue;
use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};
use crate::metrics::{B2buaMetrics, LimiterFailure, LimiterOp};

/// A [`CallLimiter`] behind the worker's breaker policy. See the module doc.
pub struct BreakerLimiter {
    inner: Arc<dyn CallLimiter>,
    state: watch::Receiver<bool>,
    releases: Arc<ReleaseQueue>,
    refreshes: Arc<RefreshBatch>,
    metrics: B2buaMetrics,
    /// The state the queue, the batch and the metrics were last set to.
    applied: Mutex<bool>,
}

impl BreakerLimiter {
    /// `inner` behind the policy that holds and resumes `releases` and
    /// `refreshes`, and the policy to [`run`](Self::run); `inner` itself and
    /// no policy when it has no breaker. The breaker is tripped open when
    /// `inner`'s address is not known.
    pub fn guard(
        inner: Arc<dyn CallLimiter>,
        releases: Arc<ReleaseQueue>,
        refreshes: Arc<RefreshBatch>,
        metrics: B2buaMetrics,
    ) -> (Arc<dyn CallLimiter>, Option<Arc<Self>>) {
        let Some(health) = inner.health() else {
            return (inner, None);
        };
        metrics.limiter().set_breaker_open(false);
        if !health.has_address() {
            health.trip();
        }
        let policy = Arc::new(Self {
            inner,
            state: health.breaker(),
            releases,
            refreshes,
            metrics,
            applied: Mutex::new(false),
        });
        policy.apply(policy.is_open());
        (policy.clone(), Some(policy))
    }

    fn is_open(&self) -> bool {
        *self.state.borrow()
    }

    /// Hold (`open`) or resume the queue and the batch, once per transition.
    fn apply(&self, open: bool) {
        let mut applied = crate::poison::locked(&self.applied, crate::poison::LIMITER_BREAKER);
        if *applied == open {
            return;
        }
        *applied = open;
        if open {
            self.releases.hold();
            self.refreshes.hold();
        } else {
            self.releases.resume();
            self.refreshes.resume();
        }
        self.metrics.limiter().count_breaker_transition(open);
        drop(applied);
    }

    /// Follow the breaker until it is gone: every transition holds or
    /// resumes the queue and the batch.
    pub async fn run(self: Arc<Self>) {
        let mut state = self.state.clone();
        loop {
            let open = *state.borrow_and_update();
            self.apply(open);
            if state.changed().await.is_err() {
                return;
            }
        }
    }
}

#[async_trait]
impl CallLimiter for BreakerLimiter {
    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        if self.is_open() {
            self.metrics.limiter().count_failure(LimiterOp::Admit, LimiterFailure::BreakerOpen);
            return AdmitOutcome::NotSent;
        }
        self.inner.admit(key, change, held, entries, release_on_refusal).await
    }

    fn admit_budget(&self) -> std::time::Duration {
        self.inner.admit_budget()
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.inner.release(keys).await
    }

    /// Open, nothing is sent: the batch keeps the calls due until the close.
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        if self.is_open() {
            return RefreshAnswer::Unavailable;
        }
        self.inner.refresh(calls).await
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}

#[cfg(test)]
mod tests;
