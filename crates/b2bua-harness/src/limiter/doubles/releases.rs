//! Decorators that fault releases.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};
use tokio::sync::watch;

/// `inner`, except that no release reaches it: each is answered `Released`
/// when `answers`, else `Unavailable` (so the caller keeps it queued).
pub fn drop_releases(answers: bool, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(ReleaseFault { fault: Fault::Drop { answers }, inner })
}

/// `inner`, except that a release waits until `gate` is open before it
/// reaches `inner`.
pub fn gate_releases(gate: &ReleaseGate, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(ReleaseFault { fault: Fault::Gate(gate.open.subscribe()), inner })
}

/// `inner`, except that the first release panics the task sending it; later
/// releases are forwarded.
pub fn panic_on_first_release(inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(ReleaseFault { fault: Fault::PanicFirst(AtomicBool::new(false)), inner })
}

/// The gate the limiters of [`gate_releases`] wait on; it starts open.
pub struct ReleaseGate {
    open: watch::Sender<bool>,
}

impl Default for ReleaseGate {
    fn default() -> Self {
        Self { open: watch::channel(true).0 }
    }
}

impl ReleaseGate {
    /// From now on every release waits.
    pub fn close(&self) {
        self.open.send_replace(false);
    }

    /// Every waiting release goes on, and later ones pass.
    pub fn open(&self) {
        self.open.send_replace(true);
    }
}

enum Fault {
    Drop {
        answers: bool,
    },
    Gate(watch::Receiver<bool>),
    /// Whether a release has been asked for yet.
    PanicFirst(AtomicBool),
}

struct ReleaseFault {
    fault: Fault,
    inner: Arc<dyn CallLimiter>,
}

#[async_trait]
impl CallLimiter for ReleaseFault {
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
        match &self.fault {
            Fault::Drop { answers: true } => return ReleaseAnswer::Released,
            Fault::Drop { answers: false } => return ReleaseAnswer::Unavailable,
            Fault::Gate(open) => {
                let _ = open.clone().wait_for(|open| *open).await;
            }
            Fault::PanicFirst(asked) => {
                assert!(asked.swap(true, Ordering::SeqCst), "the first release panics");
            }
        }
        self.inner.release(keys).await
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.inner.refresh(calls).await
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}
