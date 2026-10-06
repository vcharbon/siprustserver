//! [`BoundedLimiter`] — the one bound on an admit's wait, just above the
//! limiter client.
//!
//! An admit not answered within the limiter's admit budget plus
//! [`ADMIT_SLACK`] is ended there: it answers [`AdmitOutcome::Unavailable`],
//! counted as a [`LimiterFailure::Timeout`]. A client that bounds its own
//! wait answers first and is passed through as it answered. The worker's
//! circuit breaker sits above it and counts the stall as any other admit
//! with no usable answer. Release and refresh are forwarded unbounded: the
//! release queue, the refresh batch and the client bound those.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use crate::metrics::{B2buaMetrics, LimiterFailure, LimiterOp};

/// How long past the limiter's admit budget the worker waits for any admit
/// before it answers `unavailable`: a client's own timeout fires first and is
/// counted. Well inside the answer deadline's margin (ADR-0039).
pub const ADMIT_SLACK: Duration = Duration::from_millis(100);

/// A [`CallLimiter`] whose admits are answered by the cap. See the module doc.
pub struct BoundedLimiter {
    inner: Arc<dyn CallLimiter>,
    metrics: B2buaMetrics,
}

impl BoundedLimiter {
    /// `inner` with its admits bounded, the stalls counted on `metrics`.
    pub fn wrap(inner: Arc<dyn CallLimiter>, metrics: B2buaMetrics) -> Arc<dyn CallLimiter> {
        Arc::new(Self { inner, metrics })
    }

    /// The longest an admit waits: the admit budget plus [`ADMIT_SLACK`].
    fn cap(&self) -> Duration {
        self.inner.admit_budget() + ADMIT_SLACK
    }
}

#[async_trait]
impl CallLimiter for BoundedLimiter {
    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let admit = self.inner.admit(key, change, held, entries, release_on_refusal);
        match tokio::time::timeout(self.cap(), admit).await {
            Ok(outcome) => outcome,
            Err(_) => {
                self.metrics.limiter().count_failure(LimiterOp::Admit, LimiterFailure::Timeout);
                AdmitOutcome::Unavailable
            }
        }
    }

    fn admit_budget(&self) -> Duration {
        self.inner.admit_budget()
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.inner.release(keys).await
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.inner.refresh(calls).await
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        self.inner.health()
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}

#[cfg(test)]
mod tests {
    use sip_clock::testkit::settle;

    use super::*;
    use crate::limiter::LOCAL_ADMIT_BUDGET;

    /// A limiter answering every admit `answer` after `delay` (never when
    /// `None`).
    struct Delayed {
        delay: Option<Duration>,
        answer: AdmitOutcome,
    }

    #[async_trait]
    impl CallLimiter for Delayed {
        fn admit_budget(&self) -> Duration {
            LOCAL_ADMIT_BUDGET
        }
        async fn admit(
            &self,
            _: &str,
            _: u64,
            _: &LimiterHeld,
            _: &[LimiterEntry],
            _: bool,
        ) -> AdmitOutcome {
            match self.delay {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending().await,
            }
            self.answer.clone()
        }
        async fn release(&self, _: &[String]) -> ReleaseAnswer {
            ReleaseAnswer::Released
        }
        async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
            RefreshAnswer::Unavailable
        }
        fn report_to(&self, _: LimiterReports) {}
    }

    fn bounded(
        delay: Option<Duration>,
        answer: AdmitOutcome,
    ) -> (Arc<dyn CallLimiter>, B2buaMetrics) {
        let metrics = B2buaMetrics::new();
        (BoundedLimiter::wrap(Arc::new(Delayed { delay, answer }), metrics.clone()), metrics)
    }

    fn spawn_admit(limiter: &Arc<dyn CallLimiter>) -> tokio::task::JoinHandle<AdmitOutcome> {
        let limiter = limiter.clone();
        tokio::spawn(async move {
            let entries = [LimiterEntry { id: "x".into(), limit: 1 }];
            limiter.admit("k", 1, &LimiterHeld::default(), &entries, false).await
        })
    }

    fn timeouts(metrics: &B2buaMetrics) -> u64 {
        metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout)
    }

    #[tokio::test(start_paused = true)]
    async fn an_admit_never_answered_is_unavailable_at_the_cap_and_counted_as_a_timeout() {
        let (limiter, metrics) = bounded(None, AdmitOutcome::Admitted);
        let admit = spawn_admit(&limiter);
        settle().await;
        let cap = LOCAL_ADMIT_BUDGET + ADMIT_SLACK;
        tokio::time::advance(cap - Duration::from_millis(1)).await;
        settle().await;
        assert!(!admit.is_finished(), "awaited up to the cap");
        assert_eq!(timeouts(&metrics), 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert!(admit.is_finished(), "ended at the cap");
        assert_eq!(admit.await.unwrap(), AdmitOutcome::Unavailable);
        assert_eq!(timeouts(&metrics), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_within_the_cap_passes_through_uncounted() {
        let cap = LOCAL_ADMIT_BUDGET + ADMIT_SLACK;
        let answers = [
            AdmitOutcome::Admitted,
            AdmitOutcome::Released,
            AdmitOutcome::Unavailable,
            AdmitOutcome::Rejected { limiter_id: "x".into(), held: LimiterHeld::default() },
        ];
        for answer in answers {
            let delay = cap - Duration::from_millis(1);
            let (limiter, metrics) = bounded(Some(delay), answer.clone());
            let admit = spawn_admit(&limiter);
            settle().await;
            tokio::time::advance(delay).await;
            settle().await;
            assert_eq!(admit.await.unwrap(), answer, "as answered");
            assert_eq!(timeouts(&metrics), 0, "{answer:?} is no timeout");
        }
    }

    #[test]
    fn it_states_the_budget_and_the_health_answer_of_what_it_wraps() {
        let (limiter, _) = bounded(None, AdmitOutcome::Admitted);
        assert_eq!(limiter.admit_budget(), LOCAL_ADMIT_BUDGET);
        assert!(limiter.health().is_none());
    }
}
