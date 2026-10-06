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
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::limiter::lease::LimiterLease;
    use crate::limiter::refresh_batch::RefreshBatchConfig;
    use crate::limiter::release_queue::ReleaseQueueConfig;
    use crate::limiter::testkit::{held_of, replies};
    use crate::limiter::RefreshOutcome;
    use crate::metrics::RefreshGiveUp;

    /// A limiter answering admits from a script (then `Admitted`), logging
    /// every request, with a health answer at the test's say.
    #[derive(Default)]
    struct Scripted {
        admits: Mutex<VecDeque<AdmitOutcome>>,
        admits_sent: AtomicUsize,
        refreshed: Mutex<Vec<Vec<RefreshCall>>>,
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
        fn admit_budget(&self) -> std::time::Duration {
            crate::limiter::LOCAL_ADMIT_BUDGET
        }

        async fn admit(
            &self,
            _: &str,
            _: u64,
            _: &LimiterHeld,
            _: &[LimiterEntry],
            _: bool,
        ) -> AdmitOutcome {
            self.admits_sent.fetch_add(1, Ordering::SeqCst);
            self.admits.lock().unwrap().pop_front().unwrap_or(AdmitOutcome::Admitted)
        }
        async fn release(&self, keys: &[String]) -> ReleaseAnswer {
            self.released.lock().unwrap().push(keys.to_vec());
            ReleaseAnswer::Released
        }
        async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
            self.refreshed.lock().unwrap().push(calls.to_vec());
            RefreshAnswer::Answered(replies(calls, RefreshOutcome::Extended))
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.serving.clone())
        }
        fn report_to(&self, _: crate::limiter::LimiterReports) {}
    }

    struct Rig {
        scripted: Arc<Scripted>,
        limiter: Arc<dyn CallLimiter>,
        releases: Arc<ReleaseQueue>,
        refreshes: Arc<RefreshBatch>,
        metrics: B2buaMetrics,
    }

    /// The worker's refresh batch over `inner`, forgetting what `releases`
    /// is told, its sender running.
    fn batch(
        inner: Arc<dyn CallLimiter>,
        releases: &ReleaseQueue,
        metrics: &B2buaMetrics,
    ) -> Arc<RefreshBatch> {
        let config = RefreshBatchConfig {
            tick: PROBE,
            max: 100,
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let refreshes = RefreshBatch::new(inner, config, metrics.clone(), |_| {});
        let forget = refreshes.clone();
        assert!(releases.on_push(move |key| forget.forget(key)));
        tokio::spawn(refreshes.clone().run());
        refreshes
    }

    const PROBE: Duration = Duration::from_secs(1);

    fn rig(script: Vec<AdmitOutcome>) -> Rig {
        rig_with(script, Scripted::default())
    }

    fn rig_with(script: Vec<AdmitOutcome>, scripted: Scripted) -> Rig {
        let scripted = Arc::new(scripted);
        *scripted.admits.lock().unwrap() = script.into();
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
        tokio::spawn(releases.clone().run());
        let refreshes = batch(scripted.clone(), &releases, &metrics);
        let (limiter, breaker) = BreakerLimiter::guard(
            scripted.clone(),
            BreakerConfig { failures: 3, probe: PROBE },
            releases.clone(),
            refreshes.clone(),
            metrics.clone(),
        );
        tokio::spawn(breaker.expect("a limiter with a health answer is guarded").run());
        Rig { scripted, limiter, releases, refreshes, metrics }
    }

    async fn admit(limiter: &Arc<dyn CallLimiter>) -> AdmitOutcome {
        limiter
            .admit(
                "k",
                1,
                &LimiterHeld::default(),
                &[LimiterEntry { id: "x".into(), limit: 1 }],
                false,
            )
            .await
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
        assert!(r.metrics.limiter().breaker_open());
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "open: nothing sent");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 3);
        assert_eq!(
            r.metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen),
            1
        );
        assert_eq!(r.metrics.limiter().breaker_transitions_total(true), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn any_answered_admit_ends_the_run() {
        let mut script = lost(2);
        script.push(AdmitOutcome::Rejected { limiter_id: "x".into(), held: Default::default() });
        script.extend(lost(2));
        script.push(AdmitOutcome::Released);
        script.extend(lost(2));
        script.push(AdmitOutcome::Admitted);
        script.extend(lost(2));
        let r = rig(script);
        for _ in 0..11 {
            assert_ne!(admit(&r.limiter).await, AdmitOutcome::NotSent);
        }
        assert!(!r.metrics.limiter().breaker_open(), "no run reached three");
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
        assert!(r.metrics.limiter().breaker_open(), "two lost, one unsent, one lost");
    }

    #[tokio::test(start_paused = true)]
    async fn open_it_holds_the_refresh_batch_and_its_close_sends_every_key_due_at_once() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        r.refreshes.mark("k", "c-k", &held_of(&["x".into()]));
        r.refreshes.mark("k", "c-k", &held_of(&["x".into(), "y".into()]));
        r.refreshes.mark("other", "c-other", &held_of(&["z".into()]));
        r.refreshes.mark("ended", "c-ended", &held_of(&["x".into()]));
        r.releases.push("ended");
        for _ in 0..3 {
            tokio::time::advance(PROBE).await;
            settle().await;
        }
        assert!(r.scripted.refreshed.lock().unwrap().is_empty(), "nothing sent open");
        assert_eq!(r.metrics.limiter().refresh_due(), 2, "the ended call's is forgotten");
        assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Released), 1);

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        let key = |key: &str, ids: &[&str]| RefreshCall {
            key: key.into(),
            held: held_of(&ids.iter().map(|id| id.to_string()).collect::<Vec<_>>()),
        };
        assert_eq!(
            *r.scripted.refreshed.lock().unwrap(),
            [vec![key("k", &["x", "y"]), key("other", &["z"])]],
            "one request at the close: the latest refresh of k, none of a call whose release \
             waited"
        );
        assert_eq!(r.metrics.limiter().refresh_due(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_sent_while_open_sends_nothing() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        let calls = [RefreshCall { key: "k".into(), held: held_of(&["x".into()]) }];
        assert_eq!(r.limiter.refresh(&calls).await, RefreshAnswer::Unavailable);
        assert!(r.scripted.refreshed.lock().unwrap().is_empty());
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

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter().breaker_open(), "the first answer closes it");
        assert_eq!(r.metrics.limiter().breaker_transitions_total(false), 1);
        assert_eq!(*r.scripted.released.lock().unwrap(), [vec!["a".to_string()]]);
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted, "admits are sent again");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_given_up_at_the_cap_leaves_no_refresh_to_send_on_close() {
        let r = rig(lost(3));
        trip(&r.limiter).await;
        r.refreshes.mark("ended", "c-ended", &held_of(&["x".into()]));
        r.releases.push("ended");
        for n in 0..10 {
            r.releases.push(&format!("other-{n}"));
        }
        assert!(!r.releases.waiting_keys().contains(&"ended".to_string()), "given up at the cap");

        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter().breaker_open());
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
        assert!(!r.metrics.limiter().breaker_open());
        r.scripted.serving.up.store(false, Ordering::SeqCst);
        trip(&r.limiter).await;
        assert!(r.metrics.limiter().breaker_open(), "a new run opens it again");
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert_eq!(
            (
                r.metrics.limiter().breaker_transitions_total(true),
                r.metrics.limiter().breaker_transitions_total(false)
            ),
            (2, 2)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_limiter_without_an_address_starts_open_and_the_first_health_answer_closes_it() {
        let scripted = Scripted::default();
        scripted.serving.unaddressed.store(true, Ordering::SeqCst);
        let r = rig_with(Vec::new(), scripted);
        assert!(r.metrics.limiter().breaker_open(), "starts open");
        assert_eq!(r.metrics.limiter().breaker_transitions_total(true), 1);
        assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "fails open at once");
        assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 0, "nothing sent");
        assert_eq!(
            r.metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen),
            1
        );
        r.releases.push("a");
        settle().await;
        assert!(r.scripted.released.lock().unwrap().is_empty(), "the queue is held");

        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(r.metrics.limiter().breaker_open(), "no answer yet");
        r.scripted.serving.unaddressed.store(false, Ordering::SeqCst);
        r.scripted.serving.up.store(true, Ordering::SeqCst);
        tokio::time::advance(PROBE).await;
        settle().await;
        assert!(!r.metrics.limiter().breaker_open(), "the first answer closes it");
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
        assert!(!r.metrics.limiter().breaker_open());
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
        fn admit_budget(&self) -> std::time::Duration {
            crate::limiter::LOCAL_ADMIT_BUDGET
        }

        async fn admit(
            &self,
            key: &str,
            _: u64,
            _: &LimiterHeld,
            _: &[LimiterEntry],
            _: bool,
        ) -> AdmitOutcome {
            if key != "slow" {
                return AdmitOutcome::Unavailable;
            }
            self.go.notified().await;
            self.slow.clone()
        }
        async fn release(&self, _: &[String]) -> ReleaseAnswer {
            ReleaseAnswer::Released
        }
        async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
            RefreshAnswer::Answered(replies(calls, RefreshOutcome::Extended))
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.serving.clone())
        }
        fn report_to(&self, _: crate::limiter::LimiterReports) {}
    }

    /// `inner` behind its admit bound and a breaker opening on one lost
    /// admit, as the worker composes them, its probe running.
    fn guard_one(inner: Arc<dyn CallLimiter>) -> (Arc<dyn CallLimiter>, B2buaMetrics) {
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let releases = ReleaseQueue::new(inner.clone(), config, metrics.clone());
        let refreshes = batch(inner.clone(), &releases, &metrics);
        let (limiter, breaker) = BreakerLimiter::guard(
            crate::limiter::bounded::BoundedLimiter::wrap(inner, metrics.clone()),
            BreakerConfig { failures: 1, probe: PROBE },
            releases,
            refreshes,
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
                    limiter
                        .admit(
                            "slow",
                            2,
                            &LimiterHeld::default(),
                            &[LimiterEntry { id: "x".into(), limit: 1 }],
                            false,
                        )
                        .await
                }
            });
            settle().await;
            assert_eq!(admit(&limiter).await, AdmitOutcome::Unavailable, "this one opens it");
            assert!(metrics.limiter().breaker_open());
            inner.go.notify_one();
            assert_eq!(in_flight.await.unwrap(), slow, "its answer reaches its call");
            assert!(metrics.limiter().breaker_open(), "{slow:?} after the open closes nothing");
            assert_eq!(
                metrics.limiter().breaker_transitions_total(true),
                1,
                "{slow:?} after the open opens nothing again"
            );
            assert_eq!(admit(&limiter).await, AdmitOutcome::NotSent, "still open");
        }
    }

    /// An admit the limiter never answers is ended just past the admit
    /// budget by the bound below the breaker, counted once as a timeout, and
    /// the breaker opens on it.
    #[tokio::test(start_paused = true)]
    async fn an_admit_the_limiter_never_answers_counts_as_a_timeout() {
        let inner = Arc::new(InFlight {
            go: tokio::sync::Notify::new(),
            slow: AdmitOutcome::Admitted,
            serving: Arc::default(),
        });
        let (limiter, metrics) = guard_one(inner);
        let stalled = tokio::spawn(async move {
            limiter
                .admit(
                    "slow",
                    2,
                    &LimiterHeld::default(),
                    &[LimiterEntry { id: "x".into(), limit: 1 }],
                    false,
                )
                .await
        });
        settle().await;
        let cap = crate::limiter::LOCAL_ADMIT_BUDGET + crate::limiter::bounded::ADMIT_SLACK;
        tokio::time::advance(cap - Duration::from_millis(1)).await;
        settle().await;
        assert!(!stalled.is_finished(), "awaited up to the cap");
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert!(stalled.is_finished(), "ended at the cap");
        assert_eq!(stalled.await.unwrap(), AdmitOutcome::Unavailable);
        assert_eq!(metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout), 1);
        assert!(metrics.limiter().breaker_open(), "the stall opened the breaker");
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
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
        let refreshes = batch(scripted.clone(), &releases, &metrics);
        let inner: Arc<dyn CallLimiter> =
            Arc::new(WithHealth { inner: scripted, health: health.clone() });
        let (limiter, breaker) = BreakerLimiter::guard(
            inner,
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            refreshes,
            metrics.clone(),
        );
        tokio::spawn(breaker.unwrap().run());
        trip(&limiter).await;
        for _ in 0..5 {
            tokio::time::advance(PROBE).await;
            settle().await;
        }
        assert_eq!(health.asked.load(Ordering::SeqCst), 5, "one probe per period, no spin");
        assert_eq!(metrics.limiter().task_restarts_total(LimiterTask::BreakerProbe), 5);
        assert!(metrics.limiter().breaker_open(), "the breaker stays open");
        assert_eq!(admit(&limiter).await, AdmitOutcome::NotSent);
    }

    /// `inner` with `health` as its health answer.
    struct WithHealth {
        inner: Arc<Scripted>,
        health: Arc<dyn LimiterHealth>,
    }

    #[async_trait]
    impl CallLimiter for WithHealth {
        fn admit_budget(&self) -> std::time::Duration {
            self.inner.admit_budget()
        }

        async fn admit(
            &self,
            key: &str,
            change: u64,
            held: &LimiterHeld,
            e: &[LimiterEntry],
            r: bool,
        ) -> AdmitOutcome {
            self.inner.admit(key, change, held, e, r).await
        }
        async fn release(&self, keys: &[String]) -> ReleaseAnswer {
            self.inner.release(keys).await
        }
        async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
            self.inner.refresh(calls).await
        }
        fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
            Some(self.health.clone())
        }
        fn report_to(&self, _: crate::limiter::LimiterReports) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_guarded_limiter_is_not_guarded_again() {
        let r = rig(Vec::new());
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let releases = ReleaseQueue::new(r.limiter.clone(), config, metrics.clone());
        let refreshes = batch(r.limiter.clone(), &releases, &metrics);
        let (_, again) = BreakerLimiter::guard(
            r.limiter.clone(),
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            refreshes,
            metrics,
        );
        assert!(again.is_none(), "one breaker per limiter");
    }

    #[tokio::test]
    async fn a_limiter_without_a_health_answer_runs_without_a_breaker() {
        let metrics = B2buaMetrics::new();
        let noop: Arc<dyn CallLimiter> = Arc::new(crate::limiter::NoopLimiter);
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
        let releases = ReleaseQueue::new(noop.clone(), config, metrics.clone());
        let refreshes = batch(noop.clone(), &releases, &metrics);
        let (_, breaker) = BreakerLimiter::guard(
            noop,
            BreakerConfig { failures: 3, probe: PROBE },
            releases,
            refreshes,
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
