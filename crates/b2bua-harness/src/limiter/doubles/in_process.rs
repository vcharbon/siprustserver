//! A limiter that applies every request to a [`CallStore`] in process, with
//! no wire in between.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer, LOCAL_ADMIT_BUDGET,
};
use call_limiter::wire::{AdmitEntry, HeldSet};
use call_limiter::{AdmitResult, CallStore, RefreshResult};

/// A limiter that answers every admit, release and refresh with what `store`
/// does with it, at once.
pub fn in_process(store: Arc<CallStore>) -> Arc<dyn CallLimiter> {
    Arc::new(InProcess { store })
}

struct InProcess {
    store: Arc<CallStore>,
}

#[async_trait]
impl CallLimiter for InProcess {
    fn admit_budget(&self) -> Duration {
        LOCAL_ADMIT_BUDGET
    }

    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let carried = HeldSet { change: held.change, entries: to_wire(&held.entries) };
        match self.store.admit_carrying(
            key,
            change,
            &carried,
            &to_wire(entries),
            release_on_refusal,
        ) {
            AdmitResult::Admitted => AdmitOutcome::Admitted,
            AdmitResult::Rejected { limiter_id, held } => {
                AdmitOutcome::Rejected { limiter_id, held: from_wire(held) }
            }
            AdmitResult::Superseded { held } => AdmitOutcome::Superseded { held: from_wire(held) },
            AdmitResult::Released => AdmitOutcome::Released,
        }
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.store.release(keys);
        ReleaseAnswer::Released
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        let wire: Vec<_> = calls.iter().map(|c| to_wire(&c.held.entries)).collect();
        let named = calls.iter().zip(&wire).map(|(c, w)| (c.key.as_str(), c.held.change, &w[..]));
        let replies = self.store.refresh_all(named).into_iter().map(|refreshed| RefreshReply {
            outcome: match refreshed.result {
                RefreshResult::Extended => RefreshOutcome::Extended,
                RefreshResult::Reregistered => RefreshOutcome::Reregistered,
                RefreshResult::Released => RefreshOutcome::Released,
                RefreshResult::Dropped => RefreshOutcome::Dropped,
            },
            held: refreshed.held.map(from_wire),
        });
        RefreshAnswer::Answered(replies.collect())
    }

    fn report_to(&self, _: LimiterReports) {}
}

fn to_wire(entries: &[LimiterEntry]) -> Vec<AdmitEntry> {
    entries.iter().map(|e| AdmitEntry { id: e.id.clone(), limit: e.limit }).collect()
}

fn from_wire(held: HeldSet) -> LimiterHeld {
    let entries =
        held.entries.into_iter().map(|e| LimiterEntry { id: e.id, limit: e.limit }).collect();
    LimiterHeld { change: held.change, entries }
}
