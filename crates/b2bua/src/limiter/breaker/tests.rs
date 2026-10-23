use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::limiter::lease::LimiterLease;
use crate::limiter::refresh_batch::RefreshBatchConfig;
use crate::limiter::release_queue::ReleaseQueueConfig;
use crate::limiter::testkit::{held_of, replies};
use crate::limiter::{LimiterHealth, RefreshOutcome};
use crate::metrics::RefreshGiveUp;
use sip_clock::testkit::settle;

/// The client's breaker, switched by the test.
struct Switch {
    open: watch::Sender<bool>,
    unaddressed: AtomicBool,
    trips: AtomicUsize,
}

impl Default for Switch {
    fn default() -> Self {
        Self {
            open: watch::channel(false).0,
            unaddressed: AtomicBool::new(false),
            trips: AtomicUsize::new(0),
        }
    }
}

impl Switch {
    fn set(&self, open: bool) {
        self.open.send_replace(open);
    }
}

impl LimiterHealth for Switch {
    fn breaker(&self) -> watch::Receiver<bool> {
        self.open.subscribe()
    }
    fn has_address(&self) -> bool {
        !self.unaddressed.load(Ordering::SeqCst)
    }
    fn trip(&self) {
        self.trips.fetch_add(1, Ordering::SeqCst);
        self.set(true);
    }
}

/// A limiter answering admits from a script (then `Admitted`), logging
/// every request, its breaker at the test's say.
#[derive(Default)]
struct Scripted {
    admits: Mutex<VecDeque<AdmitOutcome>>,
    admits_sent: AtomicUsize,
    refreshed: Mutex<Vec<Vec<RefreshCall>>>,
    released: Mutex<Vec<Vec<String>>>,
    switch: Arc<Switch>,
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
        Some(self.switch.clone())
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

const TICK: Duration = Duration::from_secs(1);

/// The worker's refresh batch over `inner`, forgetting what `releases`
/// is told, its sender running.
fn batch(
    inner: Arc<dyn CallLimiter>,
    releases: &ReleaseQueue,
    metrics: &B2buaMetrics,
) -> Arc<RefreshBatch> {
    let config = RefreshBatchConfig {
        tick: TICK,
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

fn rig(scripted: Scripted) -> Rig {
    let scripted = Arc::new(scripted);
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let releases = ReleaseQueue::new(scripted.clone(), config, metrics.clone());
    tokio::spawn(releases.clone().run());
    let refreshes = batch(scripted.clone(), &releases, &metrics);
    let (limiter, policy) = BreakerLimiter::guard(
        scripted.clone(),
        releases.clone(),
        refreshes.clone(),
        metrics.clone(),
    );
    tokio::spawn(policy.expect("a limiter with a breaker is guarded").run());
    Rig { scripted, limiter, releases, refreshes, metrics }
}

async fn admit(limiter: &Arc<dyn CallLimiter>) -> AdmitOutcome {
    limiter
        .admit("k", 1, &LimiterHeld::default(), &[LimiterEntry { id: "x".into(), limit: 1 }], false)
        .await
}

/// Set the breaker and let the policy follow.
async fn set(r: &Rig, open: bool) {
    r.scripted.switch.set(open);
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn open_an_admit_sends_nothing_and_is_counted() {
    let r = rig(Scripted::default());
    assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted);
    set(&r, true).await;
    assert!(r.metrics.limiter().breaker_open());
    assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "open: nothing sent");
    assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen),
        1
    );
    assert_eq!(r.metrics.limiter().breaker_transitions_total(true), 1);
}

#[tokio::test(start_paused = true)]
async fn open_it_holds_the_refresh_batch_and_its_close_sends_every_key_due_at_once() {
    let r = rig(Scripted::default());
    set(&r, true).await;
    r.refreshes.mark("k", "c-k", &held_of(&["x".into()]));
    r.refreshes.mark("k", "c-k", &held_of(&["x".into(), "y".into()]));
    r.refreshes.mark("other", "c-other", &held_of(&["z".into()]));
    r.refreshes.mark("ended", "c-ended", &held_of(&["x".into()]));
    r.releases.push("ended");
    for _ in 0..3 {
        tokio::time::advance(TICK).await;
        settle().await;
    }
    assert!(r.scripted.refreshed.lock().unwrap().is_empty(), "nothing sent open");
    assert_eq!(r.metrics.limiter().refresh_due(), 2, "the ended call's is forgotten");
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Released), 1);

    set(&r, false).await;
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
    let r = rig(Scripted::default());
    set(&r, true).await;
    let calls = [RefreshCall { key: "k".into(), held: held_of(&["x".into()]) }];
    assert_eq!(r.limiter.refresh(&calls).await, RefreshAnswer::Unavailable);
    assert!(r.scripted.refreshed.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn open_it_holds_the_releases_and_its_close_sends_them() {
    let r = rig(Scripted::default());
    set(&r, true).await;
    r.releases.push("a");
    for _ in 0..3 {
        tokio::time::advance(TICK).await;
        settle().await;
    }
    assert!(r.scripted.released.lock().unwrap().is_empty(), "nothing sent while open");

    set(&r, false).await;
    assert!(!r.metrics.limiter().breaker_open(), "the close is followed");
    assert_eq!(r.metrics.limiter().breaker_transitions_total(false), 1);
    assert_eq!(*r.scripted.released.lock().unwrap(), [vec!["a".to_string()]]);
    assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted, "admits are sent again");
}

#[tokio::test(start_paused = true)]
async fn a_release_given_up_at_the_cap_leaves_no_refresh_to_send_on_close() {
    let r = rig(Scripted::default());
    set(&r, true).await;
    r.refreshes.mark("ended", "c-ended", &held_of(&["x".into()]));
    r.releases.push("ended");
    for n in 0..10 {
        r.releases.push(&format!("other-{n}"));
    }
    assert!(!r.releases.waiting_keys().contains(&"ended".to_string()), "given up at the cap");

    set(&r, false).await;
    assert!(!r.metrics.limiter().breaker_open());
    assert!(
        r.scripted.refreshed.lock().unwrap().is_empty(),
        "an ended call's set is never refreshed after its release"
    );
}

#[tokio::test(start_paused = true)]
async fn it_follows_every_transition() {
    let r = rig(Scripted::default());
    for open in [true, false, true, false] {
        set(&r, open).await;
        assert_eq!(r.metrics.limiter().breaker_open(), open);
    }
    assert_eq!(
        (
            r.metrics.limiter().breaker_transitions_total(true),
            r.metrics.limiter().breaker_transitions_total(false)
        ),
        (2, 2)
    );
}

#[tokio::test(start_paused = true)]
async fn a_limiter_without_an_address_trips_its_breaker_and_its_close_resumes() {
    let scripted = Scripted::default();
    scripted.switch.unaddressed.store(true, Ordering::SeqCst);
    let r = rig(scripted);
    assert_eq!(r.scripted.switch.trips.load(Ordering::SeqCst), 1, "tripped at the guard");
    assert!(r.metrics.limiter().breaker_open(), "starts open");
    assert_eq!(r.metrics.limiter().breaker_transitions_total(true), 1);
    assert_eq!(admit(&r.limiter).await, AdmitOutcome::NotSent, "fails open at once");
    assert_eq!(r.scripted.admits_sent.load(Ordering::SeqCst), 0, "nothing sent");
    r.releases.push("a");
    settle().await;
    assert!(r.scripted.released.lock().unwrap().is_empty(), "the queue is held");

    set(&r, false).await;
    assert!(!r.metrics.limiter().breaker_open());
    assert_eq!(*r.scripted.released.lock().unwrap(), [vec!["a".to_string()]]);
    assert_eq!(admit(&r.limiter).await, AdmitOutcome::Admitted);
}

/// A limiter whose admits wait for the test, then answer `slow`.
struct InFlight {
    go: tokio::sync::Notify,
    slow: AdmitOutcome,
    switch: Arc<Switch>,
}

#[async_trait]
impl CallLimiter for InFlight {
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
        Some(self.switch.clone())
    }
    fn report_to(&self, _: crate::limiter::LimiterReports) {}
}

/// `inner` behind its admit bound and the breaker policy, as the worker
/// composes them, the policy running.
fn guard_one(inner: Arc<dyn CallLimiter>) -> (Arc<dyn CallLimiter>, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let releases = ReleaseQueue::new(inner.clone(), config, metrics.clone());
    let refreshes = batch(inner.clone(), &releases, &metrics);
    let (limiter, policy) = BreakerLimiter::guard(
        crate::limiter::bounded::BoundedLimiter::wrap(inner, metrics.clone()),
        releases,
        refreshes,
        metrics.clone(),
    );
    tokio::spawn(policy.expect("guarded").run());
    (limiter, metrics)
}

#[tokio::test(start_paused = true)]
async fn an_admit_answered_after_the_breaker_opened_changes_nothing() {
    for slow in [AdmitOutcome::Unavailable, AdmitOutcome::Admitted] {
        let inner = Arc::new(InFlight {
            go: tokio::sync::Notify::new(),
            slow: slow.clone(),
            switch: Arc::default(),
        });
        let (limiter, metrics) = guard_one(inner.clone());
        let in_flight = tokio::spawn({
            let limiter = limiter.clone();
            async move { admit(&limiter).await }
        });
        settle().await;
        inner.switch.set(true);
        settle().await;
        assert!(metrics.limiter().breaker_open());
        inner.go.notify_one();
        assert_eq!(in_flight.await.unwrap(), slow, "its answer reaches its call");
        assert!(metrics.limiter().breaker_open(), "{slow:?} after the open closes nothing");
        assert_eq!(metrics.limiter().breaker_transitions_total(true), 1);
        assert_eq!(admit(&limiter).await, AdmitOutcome::NotSent, "still open");
    }
}

/// An admit the limiter never answers is ended just past the admit budget
/// by the bound below the policy, counted once as a timeout.
#[tokio::test(start_paused = true)]
async fn an_admit_the_limiter_never_answers_counts_as_a_timeout() {
    let inner = Arc::new(InFlight {
        go: tokio::sync::Notify::new(),
        slow: AdmitOutcome::Admitted,
        switch: Arc::default(),
    });
    let (limiter, metrics) = guard_one(inner);
    let stalled = tokio::spawn(async move { admit(&limiter).await });
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
}

#[tokio::test(start_paused = true)]
async fn a_guarded_limiter_is_not_guarded_again() {
    let r = rig(Scripted::default());
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let releases = ReleaseQueue::new(r.limiter.clone(), config, metrics.clone());
    let refreshes = batch(r.limiter.clone(), &releases, &metrics);
    let (_, again) = BreakerLimiter::guard(r.limiter.clone(), releases, refreshes, metrics);
    assert!(again.is_none(), "one breaker policy per limiter");
}

#[tokio::test]
async fn a_limiter_without_a_breaker_runs_unguarded() {
    let metrics = B2buaMetrics::new();
    let noop: Arc<dyn CallLimiter> = Arc::new(crate::limiter::NoopLimiter);
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let releases = ReleaseQueue::new(noop.clone(), config, metrics.clone());
    let refreshes = batch(noop.clone(), &releases, &metrics);
    let (_, policy) = BreakerLimiter::guard(noop, releases, refreshes, metrics);
    assert!(policy.is_none());
}
