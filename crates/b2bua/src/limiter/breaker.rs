//! [`BreakerLimiter`] — the worker's circuit breaker in front of its call
//! limiter (ADR-0040).
//!
//! **Closed**, every request goes to the limiter. A run of
//! [`BreakerConfig::failures`] consecutive admits with no usable answer
//! ([`AdmitOutcome::Unavailable`]: a timeout, a transport error, any
//! non-200, a bad body, or no answer at all just past the limiter's admit
//! budget, where the bound below the breaker ends the wait,
//! [`crate::limiter::bounded`]) opens it; any answered admit (admitted,
//! refused at a cap, refused by a release fence) ends the run. Only admits
//! trip it.
//!
//! **Open**, an admit sends nothing and answers [`AdmitOutcome::NotSent`] at
//! once: the call owes no release by that admit, and stays as it was (an
//! initial admit leaves it uncounted, a reroute admit of a counted call
//! leaves it counted on its old set). The worker's refresh batch and release
//! queue are held: a refresh sends nothing, the batch keeps it due. Answers
//! of admits sent before the breaker opened change nothing.
//!
//! A limiter whose address is not known ([`LimiterHealth::has_address`]: its
//! name has not resolved) is guarded by a breaker that starts open. Opening
//! and every failed probe forget the limiter's address, so the probe looks
//! its name up again and reaches a limiter that moved.
//!
//! The probe ([`BreakerLimiter::run`]) asks the limiter's health answer every
//! [`BreakerConfig::probe`] while open; the first answer closes the breaker
//! and resumes the queue and the batch: every waiting release and every
//! refresh due leave at once, and each refresh's answer reaches its call. No
//! call is a probe. A limiter without a health answer runs without a breaker,
//! and a guarded limiter has none: it is never guarded twice.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use crate::abort_on_drop::AbortOnDrop;
use crate::config::B2buaConfig;
use crate::limiter::refresh_batch::RefreshBatch;
use crate::limiter::release_queue::ReleaseQueue;
use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use crate::metrics::{B2buaMetrics, LimiterFailure, LimiterOp, LimiterTask};

/// When the breaker opens and how often it probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive admits with no usable answer that open the breaker.
    pub failures: u32,
    /// The probe period while open.
    pub probe: Duration,
}

impl BreakerConfig {
    /// The breaker `config` states.
    pub(crate) fn from_config(config: &B2buaConfig) -> Self {
        Self {
            failures: config.limiter_breaker_failures.max(1),
            probe: Duration::from_millis(config.limiter_breaker_probe_ms.max(1)),
        }
    }
}

/// Why the breaker opened.
#[derive(Clone, Copy)]
enum Opened {
    /// It was built for a limiter whose address is not known.
    NoAddress,
    /// A run of admits got no usable answer.
    Failures,
}

#[derive(Default)]
struct State {
    open: bool,
    /// Consecutive admits with no usable answer while closed.
    failures: u32,
}

/// A [`CallLimiter`] behind the worker's circuit breaker. See the module doc.
pub struct BreakerLimiter {
    inner: Arc<dyn CallLimiter>,
    health: Arc<dyn LimiterHealth>,
    config: BreakerConfig,
    releases: Arc<ReleaseQueue>,
    refreshes: Arc<RefreshBatch>,
    metrics: B2buaMetrics,
    state: Mutex<State>,
    /// Wakes the probe when the breaker opens.
    opened: Notify,
}

impl BreakerLimiter {
    /// `inner` behind a breaker that holds and resumes `releases` and
    /// `refreshes`, and the breaker to [`run`](Self::run); `inner` itself and
    /// no breaker when it has no health answer. The breaker starts open when
    /// `inner`'s address is not known.
    pub fn guard(
        inner: Arc<dyn CallLimiter>,
        config: BreakerConfig,
        releases: Arc<ReleaseQueue>,
        refreshes: Arc<RefreshBatch>,
        metrics: B2buaMetrics,
    ) -> (Arc<dyn CallLimiter>, Option<Arc<Self>>) {
        let Some(health) = inner.health() else {
            return (inner, None);
        };
        let breaker = Arc::new(Self {
            inner,
            health,
            config,
            releases,
            refreshes,
            metrics,
            state: Mutex::new(State::default()),
            opened: Notify::new(),
        });
        breaker.metrics.limiter().set_breaker_open(false);
        if !breaker.health.has_address() {
            breaker.open(breaker.lock(), Opened::NoAddress);
        }
        (breaker.clone(), Some(breaker))
    }

    /// The breaker's state. Every step leaves it whole, so a poisoned lock
    /// is taken as it is.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the breaker is open.
    fn is_open(&self) -> bool {
        self.lock().open
    }

    /// Count an admit's answer: a run of failures opens the breaker, an
    /// answer ends the run.
    fn on_admit(&self, outcome: &AdmitOutcome) {
        let mut state = self.lock();
        if state.open {
            return;
        }
        match outcome {
            AdmitOutcome::Unavailable => {
                state.failures = state.failures.saturating_add(1);
                if state.failures < self.config.failures {
                    return;
                }
                self.open(state, Opened::Failures);
            }
            AdmitOutcome::NotSent => {}
            AdmitOutcome::Admitted
            | AdmitOutcome::Rejected { .. }
            | AdmitOutcome::Superseded { .. }
            | AdmitOutcome::Released => {
                state.failures = 0;
            }
        }
    }

    /// Open the breaker: hold the queue and the batch, forget the limiter's
    /// address and wake the probe.
    fn open(&self, mut state: MutexGuard<'_, State>, why: Opened) {
        state.open = true;
        state.failures = 0;
        self.releases.hold();
        self.refreshes.hold();
        self.metrics.limiter().count_breaker_transition(true);
        drop(state);
        self.health.forget_address();
        match why {
            Opened::NoAddress => tracing::warn!(
                "call limiter breaker open: limiter address not known; admits send no request \
                 until the probe resolves it and the limiter's health answer comes back"
            ),
            Opened::Failures => tracing::warn!(
                failures = self.config.failures,
                "call limiter breaker open: admits send no request until the limiter's health \
                 answer comes back"
            ),
        }
        self.opened.notify_one();
    }

    /// Close the breaker and send what the queue and the batch held.
    fn close(&self) {
        let mut state = self.lock();
        if !state.open {
            return;
        }
        state.open = false;
        state.failures = 0;
        self.releases.resume();
        self.refreshes.resume();
        self.metrics.limiter().count_breaker_transition(false);
        drop(state);
        tracing::info!("call limiter breaker closed: the limiter's health answer is back");
    }

    /// The supervised probe, until the task is aborted with the worker. A
    /// probe that panics is logged, counted and started again; the breaker
    /// keeps its state, and the restarted probe waits one period before it
    /// asks again.
    pub async fn run(self: Arc<Self>) {
        loop {
            let mut probe = AbortOnDrop(tokio::spawn(self.clone().probe()));
            match (&mut probe.0).await {
                Err(e) if e.is_panic() => {
                    self.metrics.limiter().count_task_restart(LimiterTask::BreakerProbe);
                    tracing::error!("call limiter breaker probe panicked; restarting it");
                }
                _ => return,
            }
        }
    }

    /// Wait for the breaker to open, then ask the health answer every probe
    /// period until one comes back.
    async fn probe(self: Arc<Self>) {
        loop {
            while !self.is_open() {
                self.opened.notified().await;
            }
            let start = tokio::time::Instant::now() + self.config.probe;
            let mut tick = tokio::time::interval_at(start, self.config.probe);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            while self.is_open() {
                tick.tick().await;
                if self.health.serving().await {
                    self.close();
                } else {
                    self.health.forget_address();
                }
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
        let outcome = self.inner.admit(key, change, held, entries, release_on_refusal).await;
        self.on_admit(&outcome);
        outcome
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
