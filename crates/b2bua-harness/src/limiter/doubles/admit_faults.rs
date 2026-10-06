//! Decorators that fault admits picked by their number: admits are numbered
//! from 1 in the order they reach the decorator.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, RefreshAnswer,
    RefreshCall, ReleaseAnswer,
};

/// `inner`, except that the `n`-th admit is answered `Unavailable`: with
/// `lands` it reaches `inner` first (its answer is lost), else it never leaves.
pub fn unavailable_on_nth(
    n: usize,
    lands: bool,
    inner: Arc<dyn CallLimiter>,
) -> Arc<dyn CallLimiter> {
    faulting(Picks::Nth(n), Fault::Unavailable { lands }, inner)
}

/// `inner`, except that no admit is ever answered (nor reaches `inner`);
/// states `budget` as its admit budget, which the worker bounds each by.
pub fn never_answer_admit(budget: Duration, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    faulting(Picks::Every, Fault::NeverAnswer { budget }, inner)
}

/// `inner`, except that the `n`-th admit waits `delay` before it reaches
/// `inner`; the admit budget grows by `delay`.
pub fn delay_nth_admit(
    n: usize,
    delay: Duration,
    inner: Arc<dyn CallLimiter>,
) -> Arc<dyn CallLimiter> {
    faulting(Picks::Nth(n), Fault::SlowSend(delay), inner)
}

/// `inner`, except that the `n`-th admit's answer comes back `delay` after
/// `inner` gave it; the admit budget grows by `delay`.
pub fn answer_nth_admit_late(
    n: usize,
    delay: Duration,
    inner: Arc<dyn CallLimiter>,
) -> Arc<dyn CallLimiter> {
    faulting(Picks::Nth(n), Fault::LateAnswer(delay), inner)
}

/// `inner`, except that every admit after the `n`-th waits `delay` before it
/// reaches `inner`, a local step the admit budget does not count.
pub fn delay_admits_after(
    n: usize,
    delay: Duration,
    inner: Arc<dyn CallLimiter>,
) -> Arc<dyn CallLimiter> {
    faulting(Picks::After(n), Fault::LocalStep(delay), inner)
}

fn faulting(picks: Picks, fault: Fault, inner: Arc<dyn CallLimiter>) -> Arc<dyn CallLimiter> {
    Arc::new(AdmitFault { picks, fault, admits: AtomicUsize::new(0), inner })
}

/// Which admits the fault applies to.
#[derive(Clone, Copy)]
enum Picks {
    Nth(usize),
    After(usize),
    Every,
}

impl Picks {
    fn picks(self, n: usize) -> bool {
        match self {
            Picks::Nth(nth) => n == nth,
            Picks::After(after) => n > after,
            Picks::Every => true,
        }
    }
}

/// What a picked admit meets.
#[derive(Clone, Copy)]
enum Fault {
    Unavailable { lands: bool },
    NeverAnswer { budget: Duration },
    SlowSend(Duration),
    LateAnswer(Duration),
    LocalStep(Duration),
}

struct AdmitFault {
    picks: Picks,
    fault: Fault,
    admits: AtomicUsize,
    inner: Arc<dyn CallLimiter>,
}

#[async_trait]
impl CallLimiter for AdmitFault {
    fn admit_budget(&self) -> Duration {
        match self.fault {
            Fault::NeverAnswer { budget } => budget,
            Fault::SlowSend(d) | Fault::LateAnswer(d) => self.inner.admit_budget() + d,
            Fault::Unavailable { .. } | Fault::LocalStep(_) => self.inner.admit_budget(),
        }
    }

    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let n = self.admits.fetch_add(1, Ordering::SeqCst) + 1;
        if self.picks.picks(n) {
            match self.fault {
                Fault::Unavailable { lands } => {
                    if lands {
                        self.inner.admit(key, change, held, entries, release_on_refusal).await;
                    }
                    return AdmitOutcome::Unavailable;
                }
                Fault::NeverAnswer { .. } => return std::future::pending().await,
                Fault::SlowSend(d) | Fault::LocalStep(d) => tokio::time::sleep(d).await,
                Fault::LateAnswer(d) => {
                    let outcome =
                        self.inner.admit(key, change, held, entries, release_on_refusal).await;
                    tokio::time::sleep(d).await;
                    return outcome;
                }
            }
        }
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
