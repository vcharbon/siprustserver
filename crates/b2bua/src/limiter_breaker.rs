//! [`BreakerLimiter`] — the worker's circuit breaker in front of its call
//! limiter (ADR-0038).
//!
//! **Closed**, every request goes to the limiter. A run of
//! [`BreakerConfig::failures`] consecutive admits with no usable answer
//! ([`AdmitOutcome::Unavailable`]: a timeout, a transport error, any
//! non-200, a bad body) opens it; any answered admit (admitted, refused at a
//! cap, refused by a release fence) ends the run. Only admits trip it.
//!
//! **Open**, an admit sends nothing and answers [`AdmitOutcome::NotSent`] at
//! once: the call owes no release by that admit, and stays as it was (an
//! initial admit leaves it uncounted, a reroute admit of a counted call
//! leaves it counted on its old set). A refresh sends nothing and answers
//! [`RefreshOutcome::Unavailable`]; it is held in the [`RefreshBacklog`]. The
//! worker's release queue is held. Answers of admits sent before the breaker
//! opened change nothing.
//!
//! A limiter whose address is not known ([`LimiterHealth::has_address`]: its
//! name has not resolved) is guarded by a breaker that starts open. Opening
//! and every failed probe forget the limiter's address, so the probe looks
//! its name up again and reaches a limiter that moved.
//!
//! The probe ([`BreakerLimiter::run`]) asks the limiter's health answer every
//! [`BreakerConfig::probe`] while open; the first answer closes the breaker,
//! resumes the queue (every waiting key leaves at once) and sends the held
//! refreshes, [`FLUSH_CONCURRENCY`] at a time. A call whose release is
//! pushed has its held refresh forgotten, so an ended call's set is never
//! refreshed after its release. The flushed refreshes' answers are counted
//! apart from the calls' own refreshes: the call learns its state at its
//! next refresh. No call is a probe. A limiter without a health answer
//! runs without a breaker, and a guarded limiter has none: it is never
//! guarded twice.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::abort_on_drop::AbortOnDrop;
use crate::config::B2buaConfig;
use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, RefreshOutcome, ReleaseAnswer,
};
use crate::limiter_refresh_backlog::{RefreshBacklog, RefreshBacklogConfig};
use crate::limiter_release::ReleaseQueue;
use crate::metrics::B2buaMetrics;

/// Most held refreshes in flight at once when the breaker closes.
pub const FLUSH_CONCURRENCY: usize = 32;

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
    refreshes: Arc<RefreshBacklog>,
    metrics: B2buaMetrics,
    state: Mutex<State>,
    /// Wakes the probe when the breaker opens.
    opened: Notify,
}

impl BreakerLimiter {
    /// `inner` behind a breaker that holds and resumes `releases` and holds
    /// refreshes under `backlog`, and the breaker to [`run`](Self::run);
    /// `inner` itself and no breaker when it has no health answer. The
    /// breaker starts open when `inner`'s address is not known.
    pub fn guard(
        inner: Arc<dyn CallLimiter>,
        config: BreakerConfig,
        releases: Arc<ReleaseQueue>,
        backlog: RefreshBacklogConfig,
        metrics: B2buaMetrics,
    ) -> (Arc<dyn CallLimiter>, Option<Arc<Self>>) {
        let Some(health) = inner.health() else {
            return (inner, None);
        };
        let refreshes = Arc::new(RefreshBacklog::new(backlog, metrics.clone()));
        let forget = refreshes.clone();
        let hooked = releases.on_push(move |key| forget.forget(key));
        debug_assert!(hooked, "one breaker per release queue");
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
        breaker.metrics.set_limiter_breaker_open(false);
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
                self.open(state, Opened::Failures);
            }
            AdmitOutcome::NotSent => {}
            AdmitOutcome::Admitted | AdmitOutcome::Rejected { .. } | AdmitOutcome::Released => {
                state.failures = 0;
            }
        }
    }

    /// Open the breaker: hold the queue, forget the limiter's address and
    /// wake the probe.
    fn open(&self, mut state: MutexGuard<'_, State>, why: Opened) {
        state.open = true;
        state.failures = 0;
        self.releases.hold();
        self.metrics.set_limiter_breaker_open(true);
        self.metrics.bump_limiter_breaker_opened();
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
    /// probe that panics is logged, counted and started again; the breaker
    /// keeps its state, and the restarted probe waits one period before it
    /// asks again.
    pub async fn run(self: Arc<Self>) {
        loop {
            let mut probe = AbortOnDrop(tokio::spawn(self.clone().probe()));
            match (&mut probe.0).await {
                Err(e) if e.is_panic() => {
                    self.metrics.bump_limiter_breaker_probe_restarts();
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
                    self.flush_refreshes().await;
                } else {
                    self.metrics.bump_limiter_breaker_probe_failures();
                    self.health.forget_address();
                }
            }
        }
    }

    /// Send the refreshes held while open, [`FLUSH_CONCURRENCY`] at a time.
    /// A refresh the breaker reopened before is held again.
    async fn flush_refreshes(&self) {
        let mut held = self.refreshes.take().into_iter();
        let mut in_flight = JoinSet::new();
        loop {
            while in_flight.len() < FLUSH_CONCURRENCY {
                let Some((key, ids)) = held.next() else { break };
                if self.is_open() {
                    self.refreshes.hold(&key, &ids);
                    continue;
                }
                let inner = self.inner.clone();
                in_flight.spawn(async move { inner.refresh(&key, &ids).await });
            }
            let Some(done) = in_flight.join_next().await else { break };
            if let Ok(outcome) = done {
                self.metrics.record_limiter_breaker_refresh_flushed(match outcome {
                    RefreshOutcome::Extended => "extended",
                    RefreshOutcome::Reregistered => "reregistered",
                    RefreshOutcome::Released => "released",
                    RefreshOutcome::Dropped => "dropped",
                    RefreshOutcome::Unavailable => "unavailable",
                });
            }
        }
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
            self.refreshes.hold(key, ids);
            self.metrics.bump_limiter_breaker_refreshes_not_sent();
            return RefreshOutcome::Unavailable;
        }
        self.inner.refresh(key, ids).await
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
        refreshed: Mutex<Vec<(String, Vec<String>)>>,
        refresh_answer: Mutex<Option<RefreshOutcome>>,
        released: Mutex<Vec<Vec<String>>>,
        serving: Arc<Serving>,
    }

    #[derive(Default)]
    struct Serving {
        up: AtomicBool,
        asked: AtomicUsize,
        /// The limiter's address is not known.
        unaddressed: AtomicBool,
        forgets: AtomicUsize,
    }

    #[async_trait]
    impl LimiterHealth for Serving {
        async fn serving(&self) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.up.load(Ordering::SeqCst)
        }
        fn has_address(&self) -> bool {
            !self.unaddressed.load(Ordering::SeqCst)
        }
        fn forget_address(&self) {
            self.forgets.fetch_add(1, Ordering::SeqCst);
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
        async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
            self.refreshes_sent.fetch_add(1, Ordering::SeqCst);
            self.refreshed.lock().unwrap().push((key.to_string(), ids.to_vec()));
            self.refresh_answer.lock().unwrap().clone().unwrap_or(RefreshOutcome::Extended)
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
        rig_with(script, Scripted::default())
    }

    fn rig_with(script: Vec<AdmitOutcome>, scripted: Scripted) -> Rig {
        let scripted = Arc::new(scripted);
        *scripted.admits.lock().unwrap() = script.into();
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
        tokio::spawn(releases.clone().run());
        let (limiter, breaker) = BreakerLimiter::guard(
            scripted.clone(),
            BreakerConfig { failures: 3, probe: PROBE },
            releases.clone(),
            RefreshBacklogConfig { lease: Duration::from_secs(120), cap: 10 },
            metrics.clone(),
        );
        tokio::spawn(breaker.expect("a limiter with a health answer is guarded").run());
        Rig { scripted, limiter, releases, metrics }
    }

    async fn admit(limiter: &Arc<dyn CallLimiter>) -> AdmitOutcome {
        limiter.admit("k", &[LimiterEntry { id: "x".into(), limit: 1 }], false).await
    }

    use sip_clock::testkit::settle;

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
    async fn open_it_holds_refreshes_and_sends_the_latest_of_each_on_close() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        for ids in [vec!["x".to_string()], vec!["x".to_string(), "y".to_string()]] {
            assert_eq!(r.limiter.refresh("k", &ids).await, RefreshOutcome::Unavailable);
        }
        r.limiter.refresh("ended", &["x".into()]).await;
        r.releases.push("ended");
        assert_eq!(r.scripted.refreshes_sent.load(Ordering::SeqCst), 0, "nothing sent open");
        assert_eq!(r.metrics.limiter_breaker_refreshes_not_sent_total(), 3);
        assert_eq!(r.metrics.limiter_breaker_refreshes_held(), 1, "the ended call's is forgotten");
        assert_eq!(r.metrics.limiter_breaker_refreshes_dropped_released_total(), 1);

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert_eq!(
            *r.scripted.refreshed.lock().unwrap(),
            [("k".to_string(), vec!["x".to_string(), "y".to_string()])],
            "the latest refresh of k, none of a call whose release waited"
        );
        assert_eq!(r.metrics.limiter_breaker_refreshes_held(), 0);
        assert_eq!(r.metrics.limiter_breaker_refreshes_flushed_total("extended"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_flushed_refresh_counts_apart_from_the_calls_own() {
        let r = rig(lost(3));
        *r.scripted.refresh_answer.lock().unwrap() = Some(RefreshOutcome::Released);
        trip(&r.limiter).await;
        r.limiter.refresh("k", &["x".into()]).await;
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert_eq!(r.metrics.limiter_breaker_refreshes_flushed_total("released"), 1);
        assert_eq!(r.metrics.limiter_refresh_released_total(), 0, "not a peer's reap");
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
    async fn a_release_given_up_at_the_cap_leaves_no_refresh_to_send_on_close() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        r.limiter.refresh("ended", &["x".into()]).await;
        r.releases.push("ended");
        for n in 0..10 {
            r.releases.push(&format!("other-{n}"));
        }
        assert!(!r.releases.waiting_keys().contains(&"ended".to_string()), "given up at the cap");

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter_breaker_open());
        assert!(
            r.scripted.refreshed.lock().unwrap().is_empty(),
            "an ended call's set is never refreshed after its release"
        );
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
    async fn a_limiter_without_an_address_starts_open_and_the_first_health_answer_closes_it() {
        let scripted = Scripted::default();
        scripted.serving.unaddressed.store(true, Ordering::SeqCst);
        let r = rig_with(Vec::new(), scripted);
        assert!(r.metrics.limiter_breaker_open(), "starts open");
        assert_eq!(r.metrics.limiter_breaker_opened_total(), 1);
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "fails open at once");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 0, "nothing sent");
        assert_eq!(r.metrics.limiter_breaker_admits_not_sent_total(), 1);
        r.releases.push("a");
        settle().await;
        assert!(r.scripted.released.lock().unwrap().is_empty(), "the queue is held");

        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(r.metrics.limiter_breaker_open(), "no answer yet");
        r.scripted.serving.unaddressed.store(false, Ordering::SeqCst);
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter_breaker_open(), "the first answer closes it");
        assert_eq!(*r.scripted.released.lock().unwrap(), [vec!["a".to_string()]]);
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted);
    }

    #[tokio::test(start_paused = true)]
    async fn it_forgets_the_address_when_it_opens_and_after_each_failed_probe() {
        let r = rig([vec![AdmitOutcome::Admitted], lost(3)].concat());
        admit(&r.limiter).await;
        assert_eq!(r.scripted.serving.forgets.load(Ordering::SeqCst), 0, "closed: kept");
        trip(&r.limiter).await;
        assert_eq!(r.scripted.serving.forgets.load(Ordering::SeqCst), 1, "forgotten on open");
        for _ in 0..3 {
            tokio::time::advance(PROBE).await;
            settle().await;
        }
        assert_eq!(
            r.scripted.serving.forgets.load(Ordering::SeqCst),
            4,
            "and on each failed probe"
        );
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter_breaker_open());
        assert_eq!(r.scripted.serving.forgets.load(Ordering::SeqCst), 4, "kept once answered");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_probed_while_closed() {
        let r = rig(Vec::new());
        tokio::time::advance(10 * PROBE).await;
        settle().await;
        assert_eq!(r.scripted.serving.asked.load(Ordering::SeqCst), 0);
    }

    /// A limiter whose admits of key `slow` wait for the test and then
    /// answer `slow`; every other admit is lost at once.
    struct InFlight {
        go: tokio::sync::Notify,
        slow: AdmitOutcome,
        serving: Arc<Serving>,
    }

    #[async_trait]
    impl CallLimiter for InFlight {
        async fn admit(&self, key: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
            if key != "slow" {
                return AdmitOutcome::Unavailable;
            }
            self.go.notified().await;
            self.slow.clone()
        }
        async fn release(&self, _: &[String]) -> ReleaseAnswer {
            ReleaseAnswer::Released
        }
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            RefreshOutcome::Extended
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.serving.clone())
        }
    }

    /// `inner` behind a breaker opening on one lost admit, its probe running.
    fn guard_one(inner: Arc<dyn CallLimiter>) -> (Arc<dyn CallLimiter>, B2buaMetrics) {
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(inner.clone(), config, metrics.clone());
        let (limiter, breaker) = BreakerLimiter::guard(
            inner,
            BreakerConfig { failures: 1, probe: PROBE },
            releases,
            RefreshBacklogConfig { lease: Duration::from_secs(120), cap: 10 },
            metrics.clone(),
        );
        tokio::spawn(breaker.expect("guarded").run());
        (limiter, metrics)
    }

    #[tokio::test(start_paused = true)]
    async fn an_admit_answered_after_the_breaker_opened_changes_nothing() {
        for slow in [AdmitOutcome::Unavailable, AdmitOutcome::Admitted] {
            let inner = Arc::new(InFlight {
                go: tokio::sync::Notify::new(),
                slow: slow.clone(),
                serving: Arc::default(),
            });
            let (limiter, metrics) = guard_one(inner.clone());
            let in_flight = tokio::spawn({
                let limiter = limiter.clone();
                async move {
                    limiter.admit("slow", &[LimiterEntry { id: "x".into(), limit: 1 }], false).await
                }
            });
            settle().await;
            assert_eq!(admit(&limiter).await, AdmitOutcome::Unavailable, "this one opens it");
            assert!(metrics.limiter_breaker_open());
            inner.go.notify_one();
            assert_eq!(in_flight.await.unwrap(), slow, "its answer reaches its call");
            assert!(metrics.limiter_breaker_open(), "{slow:?} after the open closes nothing");
            assert_eq!(
                metrics.limiter_breaker_opened_total(),
                1,
                "{slow:?} after the open opens nothing again"
            );
            assert_eq!(admit(&limiter).await, AdmitOutcome::NotSent, "still open");
        }
    }

    /// A health answer that panics every time it is asked.
    #[derive(Default)]
    struct PanickingHealth {
        asked: AtomicUsize,
    }

    #[async_trait]
    impl LimiterHealth for PanickingHealth {
        async fn serving(&self) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            panic!("the health answer panics");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_keeps_panicking_restarts_once_per_period_and_the_breaker_stays_open() {
        let health = Arc::new(PanickingHealth::default());
        let scripted = Arc::new(Scripted::default());
        *scripted.admits.lock().unwrap() = lost(3).into();
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
        let inner: Arc<dyn CallLimiter> =
            Arc::new(WithHealth { inner: scripted, health: health.clone() });
        let (limiter, breaker) = BreakerLimiter::guard(
            inner,
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            RefreshBacklogConfig { lease: Duration::from_secs(120), cap: 10 },
            metrics.clone(),
        );
        tokio::spawn(breaker.unwrap().run());
        trip(&limiter).await;
        for _ in 0..5 {
            tokio::time::advance(PROBE).await;
            settle().await;
        }
        assert_eq!(health.asked.load(Ordering::SeqCst), 5, "one probe per period, no spin");
        assert_eq!(metrics.limiter_breaker_probe_restarts_total(), 5);
        assert!(metrics.limiter_breaker_open(), "the breaker stays open");
        assert_eq!(admit(&limiter).await, AdmitOutcome::NotSent);
    }

    /// `inner` with `health` as its health answer.
    struct WithHealth {
        inner: Arc<Scripted>,
        health: Arc<dyn LimiterHealth>,
    }

    #[async_trait]
    impl CallLimiter for WithHealth {
        async fn admit(&self, key: &str, e: &[LimiterEntry], r: bool) -> AdmitOutcome {
            self.inner.admit(key, e, r).await
        }
        async fn release(&self, keys: &[String]) -> ReleaseAnswer {
            self.inner.release(keys).await
        }
        async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
            self.inner.refresh(key, ids).await
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.health.clone())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_guarded_limiter_is_not_guarded_again() {
        let r = rig(Vec::new());
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let releases = ReleaseQueue::new(r.limiter.clone(), config, metrics.clone());
        let (_, again) = BreakerLimiter::guard(
            r.limiter.clone(),
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            RefreshBacklogConfig { lease: Duration::from_secs(120), cap: 10 },
            metrics,
        );
        assert!(again.is_none(), "one breaker per limiter");
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
            RefreshBacklogConfig { lease: Duration::from_secs(120), cap: 10 },
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
