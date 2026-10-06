//! The plain forwarding decorator.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};

/// `inner` as it is, except that it states no health answer.
pub fn without_breaker(inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(Forward { inner })
}

struct Forward {
    inner: Arc<dyn CallLimiter>,
}

#[async_trait]
impl CallLimiter for Forward {
    fn admit_budget(&self) -> Duration {
        self.inner.admit_budget()
    }

    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        self.inner.admit(key, change, held, entries, release_on_refusal).await
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.inner.release(keys).await
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.inner.refresh(calls).await
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}
