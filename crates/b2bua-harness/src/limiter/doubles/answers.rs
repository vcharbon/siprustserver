//! Standalone limiters that answer every request one way.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer, LOCAL_ADMIT_BUDGET,
};

/// A limiter that grants every set, extends every refresh and answers every
/// release.
pub fn admit_all() -> Arc<dyn CallLimiter> {
    Arc::new(Answers { admit: AdmitOutcome::Admitted, refresh: Some(RefreshOutcome::Extended) })
}

/// A limiter that answers no admit or refresh (each `Unavailable`: the
/// caller fails open) and answers every release.
pub fn fail_open() -> Arc<dyn CallLimiter> {
    Arc::new(Answers { admit: AdmitOutcome::Unavailable, refresh: None })
}

/// A limiter behind a release fence: every admit and every refresh is
/// answered `Released`, every release is answered.
pub fn answers_released() -> Arc<dyn CallLimiter> {
    Arc::new(Answers { admit: AdmitOutcome::Released, refresh: Some(RefreshOutcome::Released) })
}

/// Answers every admit `admit`, every refresh with `refresh` per call
/// (`Unavailable` when `None`), every release `Released`.
struct Answers {
    admit: AdmitOutcome,
    refresh: Option<RefreshOutcome>,
}

#[async_trait]
impl CallLimiter for Answers {
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
        self.admit.clone()
    }

    async fn release(&self, _: &[String]) -> ReleaseAnswer {
        ReleaseAnswer::Released
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        match self.refresh {
            Some(outcome) => RefreshAnswer::Answered(
                calls.iter().map(|c| RefreshReply::restating(c, outcome)).collect(),
            ),
            None => RefreshAnswer::Unavailable,
        }
    }

    fn report_to(&self, _: LimiterReports) {}
}
