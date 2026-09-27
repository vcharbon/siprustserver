//! [`BreakerLimiter`] — the worker's circuit breaker in front of its call
//! limiter (ADR-0038).
//!
//! **Closed**, every request goes to the limiter. A run of
//! [`BreakerConfig::failures`] consecutive admits with no usable answer
//! ([`AdmitOutcome::Unavailable`]: a timeout, a transport error, a non-200, a
//! bad body) opens it; any answered admit (admitted, refused at a cap,
//! refused by a release fence) ends the run. Only admits trip it.
//!
//! **Open**, an admit sends nothing and answers [`AdmitOutcome::NotSent`] at
//! once, so the call runs uncounted and owes no release by that admit; a
//! refresh sends nothing and answers [`RefreshOutcome::Unavailable`], so a
//! counted call stays counted and refreshes again one period later; the
//! worker's release queue is held. Answers of admits sent before the breaker
//! opened change nothing.
//!
//! The probe ([`BreakerLimiter::run`]) asks the limiter's health answer every
//! [`BreakerConfig::probe`] while open; the first answer closes the breaker
//! and resumes the queue, which sends every waiting key at once. No call is
//! a probe. A limiter without a health answer runs without a breaker.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use crate::config::B2buaConfig;
use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, RefreshOutcome, ReleaseAnswer,
};
use crate::limiter_release::ReleaseQueue;
use crate::metrics::B2buaMetrics;

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
    pub fn from_config(config: &B2buaConfig) -> Self {
        Self {
            failures: config.limiter_breaker_failures.max(1),
            probe: Duration::from_millis(config.limiter_breaker_probe_ms.max(1)),
        }
    }
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
    metrics: B2buaMetrics,
    state: Mutex<State>,
    /// Wakes the probe when the breaker opens.
    opened: Notify,
}

impl BreakerLimiter {
    /// `inner` behind a breaker that holds and resumes `releases`, and the
    /// breaker to [`run`](Self::run); `inner` itself and no breaker when it
    /// has no health answer.
    pub fn guard(
        inner: Arc<dyn CallLimiter>,
        config: BreakerConfig,
        releases: Arc<ReleaseQueue>,
        metrics: B2buaMetrics,
    ) -> (Arc<dyn CallLimiter>, Option<Arc<Self>>) {
        let Some(health) = inner.health() else {
            return (inner, None);
        };
        metrics.set_limiter_breaker_open(false);
        let breaker = Arc::new(Self {
            inner,
            health,
            config,
            releases,
            metrics,
            state: Mutex::new(State::default()),
            opened: Notify::new(),
        });
        (breaker.clone(), Some(breaker))
    }

    /// The breaker's state. Every step leaves it whole, so a poisoned lock
    /// is taken as it is.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the breaker is open.
    pub fn is_open(&self) -> bool {
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
                state.open = true;
                state.failures = 0;
                self.releases.hold();
                self.metrics.set_limiter_breaker_open(true);
                self.metrics.bump_limiter_breaker_opened();
                drop(state);
                tracing::warn!(
                    failures = self.config.failures,
                    "call limiter breaker open: admits send no request until the limiter's \
                     health answer comes back"
                );
                self.opened.notify_one();
            }
            AdmitOutcome::NotSent => {}
            AdmitOutcome::Admitted | AdmitOutcome::Rejected { .. } | AdmitOutcome::Released => {
                state.failures = 0;
            }
        }
    }

    /// Close the breaker and send what the queue held.
    fn close(&self) {
        let mut state = self.lock();
        if !state.open {
            return;
        }
        state.open = false;
        state.failures = 0;
        self.releases.resume();
        self.metrics.set_limiter_breaker_open(false);
        self.metrics.bump_limiter_breaker_closed();
        drop(state);
        tracing::info!("call limiter breaker closed: the limiter's health answer is back");
    }

    /// The supervised probe, until the task is aborted with the worker. A
    /// probe that panics is logged and started again; the breaker keeps its
    /// state.
    pub async fn run(self: Arc<Self>) {
        loop {
            let mut probe = AbortOnDrop(tokio::spawn(self.clone().probe()));
            match (&mut probe.0).await {
                Err(e) if e.is_panic() => {
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
                    self.metrics.bump_limiter_breaker_probe_failures();
                }
            }
        }
    }
}

/// Aborts the task it holds when dropped: the probe dies with its
/// supervisor.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[async_trait]
impl CallLimiter for BreakerLimiter {
    async fn admit(
        &self,
        key: &str,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        if self.is_open() {
            self.metrics.bump_limiter_breaker_admits_not_sent();
            return AdmitOutcome::NotSent;
        }
        let outcome = self.inner.admit(key, entries, release_on_refusal).await;
        self.on_admit(&outcome);
        outcome
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.inner.release(keys).await
    }

    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
        if self.is_open() {
            self.metrics.bump_limiter_breaker_refreshes_not_sent();
            return RefreshOutcome::Unavailable;
        }
        self.inner.refresh(key, ids).await
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        Some(self.health.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::limiter_release::ReleaseQueueConfig;

    /// A limiter answering admits from a script (then `Admitted`), logging
    /// every request, with a health answer at the test's say.
    #[derive(Default)]
    struct Scripted {
        admits: Mutex<VecDeque<AdmitOutcome>>,
        admits_sent: AtomicUsize,
        refreshes_sent: AtomicUsize,
        released: Mutex<Vec<Vec<String>>>,
        serving: Arc<Serving>,
    }

    #[derive(Default)]
    struct Serving {
        up: AtomicBool,
        asked: AtomicUsize,
    }

    #[async_trait]
    impl LimiterHealth for Serving {
        async fn serving(&self) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.up.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CallLimiter for Scripted {
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
            self.admits_sent.fetch_add(1, Ordering::SeqCst);
            self.admits.lock().unwrap().pop_front().unwrap_or(AdmitOutcome::Admitted)
        }
        async fn release(&self, keys: &[String]) -> ReleaseAnswer {
            self.released.lock().unwrap().push(keys.to_vec());
            ReleaseAnswer::Released
        }
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            self.refreshes_sent.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::Extended
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.serving.clone())
        }
    }

    struct Rig {
        scripted: Arc<Scripted>,
        limiter: Arc<dyn CallLimiter>,
        releases: Arc<ReleaseQueue>,
        metrics: B2buaMetrics,
    }

    const PROBE: Duration = Duration::from_secs(1);

    fn rig(script: Vec<AdmitOutcome>) -> Rig {
        let scripted = Arc::new(Scripted::default());
        *scripted.admits.lock().unwrap() = script.into();
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
        tokio::spawn(releases.clone().run());
        let (limiter, breaker) = BreakerLimiter::guard(
            scripted.clone(),
            BreakerConfig { failures: 3, probe: PROBE },
            releases.clone(),
            metrics.clone(),
        );
        tokio::spawn(breaker.expect("a limiter with a health answer is guarded").run());
        Rig { scripted, limiter, releases, metrics }
    }

    async fn admit(limiter: &Arc<dyn CallLimiter>) -> AdmitOutcome {
        limiter.admit("k", &[LimiterEntry { id: "x".into(), limit: 1 }], false).await
    }

    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// Three lost admits, then let the probe see the breaker open.
    async fn trip(limiter: &Arc<dyn CallLimiter>) {
        for _ in 0..3 {
            admit(limiter).await;
        }
        settle().await;
    }

    fn lost(n: usize) -> Vec<AdmitOutcome> {
        vec![AdmitOutcome::Unavailable; n]
    }

    #[tokio::test(start_paused = true)]
    async fn three_consecutive_lost_admits_open_it() {
        let r = rig(lost(3));
        for _ in 0..3 {
            assert_eq!(admit(&r.limiter).await, AdmitOutcome::Unavailable);
        }
        assert!(r.metrics.limiter_breaker_open());
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "open: nothing sent");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 3);
        assert_eq!(r.metrics.limiter_breaker_admits_not_sent_total(), 1);
        assert_eq!(r.metrics.limiter_breaker_opened_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn any_answered_admit_ends_the_run() {
        let mut script = lost(2);
        script.push(AdmitOutcome::Rejected { limiter_id: "x".into() });
        script.extend(lost(2));
        script.push(AdmitOutcome::Released);
        script.extend(lost(2));
        script.push(AdmitOutcome::Admitted);
        script.extend(lost(2));
        let r = rig(script);
        for _ in 0..11 {
            assert_ne!(admit(&r.limiter).await, AdmitOutcome::NotSent);
        }
        assert!(!r.metrics.limiter_breaker_open(), "no run reached three");
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unsent_admit_neither_counts_nor_ends_the_run() {
        let mut script = lost(2);
        script.push(AdmitOutcome::NotSent);
        script.extend(lost(1));
        let r = rig(script);
        for _ in 0..4 {
            admit(&r.limiter).await;
        }
        assert!(r.metrics.limiter_breaker_open(), "two lost, one unsent, one lost");
    }

    #[tokio::test(start_paused = true)]
    async fn open_it_skips_refreshes() {
        let r = rig(lost(3));
        for _ in 0..3 {
            admit(&r.limiter).await;
        }
        assert_eq!(r.limiter.refresh("k", &["x".into()]).await, RefreshOutcome::Unavailable);
        assert_eq!(r.scripted.refreshes_sent.load(Ordering::SeqCst), 0);
        assert_eq!(r.metrics.limiter_breaker_refreshes_not_sent_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn open_it_holds_the_releases_and_the_first_health_answer_sends_them() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        r.releases.push("a");
        for _ in 0..3 {
            tokio::time::advance(PROBE).await;
            settle().await;
        }
        assert!(r.scripted.released.lock().unwrap().is_empty(), "nothing sent while open");
        assert_eq!(r.scripted.serving.asked.load(Ordering::SeqCst), 3, "probed every period");
        assert_eq!(r.metrics.limiter_breaker_probe_failures_total(), 3);

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter_breaker_open(), "the first answer closes it");
        assert_eq!(r.metrics.limiter_breaker_closed_total(), 1);
        assert_eq!(*r.scripted.released.lock().unwrap(), [vec!["a".to_string()]]);
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted, "admits are sent again");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn it_reopens_after_a_close() {
        let r = rig([lost(3), lost(3)].concat());
        trip(&r.limiter).await;
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter_breaker_open());
        r.scripted.serving.up.store(false, Ordering::SeqCst);
        trip(&r.limiter).await;
        assert!(r.metrics.limiter_breaker_open(), "a new run opens it again");
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert_eq!(
            (r.metrics.limiter_breaker_opened_total(), r.metrics.limiter_breaker_closed_total()),
            (2, 2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_probed_while_closed() {
        let r = rig(Vec::new());
        tokio::time::advance(10 * PROBE).await;
        settle().await;
        assert_eq!(r.scripted.serving.asked.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_limiter_without_a_health_answer_runs_without_a_breaker() {
        let metrics = B2buaMetrics::new();
        let noop: Arc<dyn CallLimiter> = Arc::new(crate::limiter::NoopLimiter);
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(noop.clone(), config, metrics.clone());
        let (_, breaker) = BreakerLimiter::guard(
            noop,
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            metrics,
        );
        assert!(breaker.is_none());
    }

    #[test]
    fn the_config_states_the_thresholds() {
        let config = B2buaConfig::default();
        assert_eq!(
            BreakerConfig::from_config(&config),
            BreakerConfig { failures: 3, probe: Duration::from_secs(1) }
        );
    }
}
