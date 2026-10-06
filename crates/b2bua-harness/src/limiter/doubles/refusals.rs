//! Decorators that refuse admits on a cap.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};

/// `inner`, except that an admit naming `id` is refused on that entry,
/// stating no set held for the call (whatever it carried or
/// `release_on_refusal` asks); the refused admit never reaches `inner`. Any
/// other admit is forwarded.
pub fn refuse_on(id: &str, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(Refuses { on: Some(id.to_string()), inner })
}

/// `inner`, except that every admit naming at least one entry is refused on
/// its first entry, stating no set held for the call (whatever it carried or
/// `release_on_refusal` asks); the refused admit never reaches `inner`. An
/// admit of the empty set is forwarded.
pub fn refuse_all(inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(Refuses { on: None, inner })
}

/// Refuses the entry `on` names, or the first entry when `on` is `None`.
struct Refuses {
    on: Option<String>,
    inner: Arc<dyn CallLimiter>,
}

impl Refuses {
    fn refused<'a>(&self, entries: &'a [LimiterEntry]) -> Option<&'a LimiterEntry> {
        match &self.on {
            Some(id) => entries.iter().find(|e| &e.id == id),
            None => entries.first(),
        }
    }
}

#[async_trait]
impl CallLimiter for Refuses {
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
        match self.refused(entries) {
            Some(e) => {
                AdmitOutcome::Rejected { limiter_id: e.id.clone(), held: Default::default() }
            }
            None => self.inner.admit(key, change, held, entries, release_on_refusal).await,
        }
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
