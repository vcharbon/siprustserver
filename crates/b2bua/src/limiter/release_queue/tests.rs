use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::*;
use crate::limiter::{AdmitOutcome, LimiterEntry, LimiterHeld, RefreshAnswer, RefreshCall};

/// A limiter whose releases answer or fail at the test's say, logging
/// the keys of every release request.
#[derive(Default)]
struct Scripted {
    down: AtomicBool,
    sent: Mutex<Vec<Vec<String>>>,
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
        AdmitOutcome::NotSent
    }
    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        self.sent.lock().unwrap().push(keys.to_vec());
        if self.down.load(Ordering::SeqCst) {
            ReleaseAnswer::Unavailable
        } else {
            ReleaseAnswer::Released
        }
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: crate::limiter::LimiterReports) {}
}

fn queue(limiter: Arc<Scripted>, cap: usize) -> (Arc<ReleaseQueue>, B2buaMetrics) {
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(20)), cap };
    let q = ReleaseQueue::new(limiter, config, metrics.clone());
    tokio::spawn(q.clone().run());
    (q, metrics)
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn sent(limiter: &Scripted) -> Vec<Vec<String>> {
    limiter.sent.lock().unwrap().clone()
}

fn keys(v: &[&str]) -> Vec<String> {
    v.iter().map(|k| k.to_string()).collect()
}

#[tokio::test(start_paused = true)]
async fn a_push_is_sent_at_once_and_leaves_the_queue() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"])]);
    assert_eq!(q.waiting(), 0);
    assert_eq!(metrics.limiter().release_queue_depth(), 0);
}

/// Answers a release only once the test lets it go.
#[derive(Default)]
struct Gated {
    go: tokio::sync::Notify,
}

#[async_trait]
impl CallLimiter for Gated {
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
        AdmitOutcome::NotSent
    }
    async fn release(&self, _: &[String]) -> ReleaseAnswer {
        self.go.notified().await;
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: crate::limiter::LimiterReports) {}
}

#[tokio::test(start_paused = true)]
async fn the_queue_says_while_its_request_is_in_flight() {
    let limiter = Arc::new(Gated::default());
    let q = ReleaseQueue::new(
        limiter.clone(),
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(20)), cap: 10 },
        B2buaMetrics::new(),
    );
    tokio::spawn(q.clone().run());
    assert!(!q.sending());
    q.push("a");
    settle().await;
    assert!(q.sending(), "the release request is in flight");
    limiter.go.notify_one();
    settle().await;
    assert!(!q.sending());
    assert_eq!(q.waiting(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_failed_send_backs_off_and_resends_every_key_in_one_batch() {
    let limiter = Arc::new(Scripted::default());
    limiter.down.store(true, Ordering::SeqCst);
    let (q, metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    q.push("b");
    q.push("c");
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"])], "b and c wait for the backoff");
    assert_eq!(metrics.limiter().release_retries_total(), 1);

    limiter.down.store(false, Ordering::SeqCst);
    tokio::time::advance(BACKOFF_INITIAL).await;
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"]), keys(&["a", "b", "c"])]);
    assert_eq!(q.waiting(), 0);
}

#[test]
fn the_backoff_doubles_up_to_its_cap() {
    assert_eq!(backoff(1), BACKOFF_INITIAL);
    assert_eq!(backoff(2), BACKOFF_INITIAL * 2);
    assert_eq!(backoff(3), BACKOFF_INITIAL * 4);
    assert_eq!(backoff(40), BACKOFF_MAX);
}

#[tokio::test(start_paused = true)]
async fn a_key_already_waiting_stays_one_entry() {
    let limiter = Arc::new(Scripted::default());
    let (q, _metrics) = queue(limiter.clone(), 10);
    q.hold();
    q.push("a");
    q.push("a");
    assert_eq!(q.waiting(), 1);
    q.resume();
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"])]);
}

#[tokio::test(start_paused = true)]
async fn an_entry_past_its_lease_is_dropped_unsent() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 10);
    q.hold();
    q.push("a");
    tokio::time::advance(Duration::from_secs(10)).await;
    q.push("b");
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(q.waiting(), 1, "a waited one lease");
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired), 1);
    q.resume();
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["b"])], "a is never sent");
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_drops_its_oldest_entry() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 2);
    q.hold();
    q.push("a");
    q.push("b");
    q.push("c");
    assert_eq!(q.waiting(), 2);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Cap), 1);
    assert_eq!(metrics.limiter().release_queue_depth(), 2);
    q.resume();
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["b", "c"])]);
}

#[tokio::test(start_paused = true)]
async fn a_held_queue_sends_nothing_and_flushes_in_one_batch_on_resume() {
    let limiter = Arc::new(Scripted::default());
    limiter.down.store(true, Ordering::SeqCst);
    let (q, _metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    q.hold();
    q.push("b");
    tokio::time::advance(BACKOFF_MAX * 2).await;
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"])], "nothing leaves while held");

    limiter.down.store(false, Ordering::SeqCst);
    q.resume();
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"]), keys(&["a", "b"])]);
    assert_eq!(q.waiting(), 0);
}

/// Panics on its first release, answers every later one.
#[derive(Default)]
struct PanicsOnce {
    calls: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl CallLimiter for PanicsOnce {
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
        AdmitOutcome::NotSent
    }
    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        let first = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(keys.to_vec());
            calls.len() == 1
        };
        assert!(!first, "the drainer's first send panics");
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: crate::limiter::LimiterReports) {}
}

#[tokio::test(start_paused = true)]
async fn a_panicking_drainer_is_restarted_with_the_queue_intact() {
    let limiter = Arc::new(PanicsOnce::default());
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(20)), cap: 10 };
    let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
    tokio::spawn(q.clone().run());
    q.push("a");
    settle().await;
    q.push("b");
    settle().await;
    assert_eq!(metrics.limiter().task_restarts_total(LimiterTask::ReleaseSender), 1);
    assert_eq!(q.waiting(), 2, "the panic counts as a failed send: both wait");
    tokio::time::advance(BACKOFF_INITIAL).await;
    settle().await;
    assert_eq!(q.waiting(), 0, "the restarted drainer sent what the first one held");
    let calls = limiter.calls.lock().unwrap().clone();
    assert_eq!(calls.first(), Some(&keys(&["a"])), "the send that panicked");
    assert!(calls[1..].concat().contains(&"a".to_string()), "a is sent again");
    assert!(calls[1..].concat().contains(&"b".to_string()));
}

/// Panics on every release while `panics` is set, answers otherwise.
#[derive(Default)]
struct PanicsWhileSet {
    panics: AtomicBool,
    answered: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl CallLimiter for PanicsWhileSet {
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
        AdmitOutcome::NotSent
    }
    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        assert!(!self.panics.load(Ordering::SeqCst), "the limiter client panics");
        self.answered.lock().unwrap().push(keys.to_vec());
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: crate::limiter::LimiterReports) {}
}

#[tokio::test(start_paused = true)]
async fn a_sender_that_keeps_panicking_restarts_at_the_backoff_pace() {
    let limiter = Arc::new(PanicsWhileSet::default());
    limiter.panics.store(true, Ordering::SeqCst);
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
    tokio::spawn(q.clone().run());
    q.push("a");
    for _ in 0..600 {
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }
    let restarts = metrics.limiter().task_restarts_total(LimiterTask::ReleaseSender);
    // 200 ms doubling to 5 s: 5 steps to the cap, then one per 5 s.
    assert!((5..=20).contains(&restarts), "{restarts} restarts in 60 s");
    assert_eq!(q.waiting_keys(), ["a"], "the queue is intact");

    limiter.panics.store(false, Ordering::SeqCst);
    tokio::time::advance(BACKOFF_MAX).await;
    settle().await;
    assert_eq!(*limiter.answered.lock().unwrap(), [keys(&["a"])]);
    assert_eq!(q.waiting(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_poisoned_queue_still_takes_and_sends_releases() {
    let limiter = Arc::new(Scripted::default());
    let (q, _metrics) = queue(limiter.clone(), 10);
    let poisoner = q.clone();
    let _ = std::thread::spawn(move || {
        let _held = poisoner.waiting.lock().unwrap();
        panic!("poison the queue's lock");
    })
    .join();
    assert!(q.waiting.is_poisoned());
    q.push("a");
    settle().await;
    assert_eq!(sent(&limiter), [keys(&["a"])]);
    assert_eq!(q.waiting(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_lease_learnt_after_a_push_moves_its_deadline() {
    let limiter = Arc::new(Scripted::default());
    let metrics = B2buaMetrics::new();
    let lease = LimiterLease::starting_at(Duration::from_secs(20));
    let config = ReleaseQueueConfig { lease: lease.clone(), cap: 10 };
    let q = ReleaseQueue::new(limiter, config, metrics.clone());
    q.hold();
    q.push("a");
    lease.learn(Duration::from_secs(60));
    tokio::time::advance(Duration::from_secs(30)).await;
    q.push("b");
    assert_eq!(q.waiting_keys(), ["a", "b"], "a longer lease keeps the entry");
    lease.learn(Duration::from_secs(10));
    tokio::time::advance(Duration::from_secs(10)).await;
    q.push("c");
    assert_eq!(q.waiting_keys(), ["c"], "a shorter lease gives up both at once");
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired), 2);
}

#[tokio::test(start_paused = true)]
async fn a_held_queue_gives_up_its_entries_at_a_shorter_lease_learnt_while_held() {
    let limiter = Arc::new(Scripted::default());
    let metrics = B2buaMetrics::new();
    let lease = LimiterLease::starting_at(Duration::from_secs(120));
    let config = ReleaseQueueConfig { lease: lease.clone(), cap: 10 };
    let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
    tokio::spawn(q.clone().run());
    q.hold();
    q.push("a");
    settle().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    lease.learn(Duration::from_secs(30));
    settle().await;
    assert_eq!(metrics.limiter().release_queue_depth(), 1, "inside the new lease");
    tokio::time::advance(Duration::from_secs(20)).await;
    settle().await;
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired),
        1,
        "at 30 s, not 120 s"
    );
    assert_eq!(metrics.limiter().release_queue_depth(), 0);
    lease.learn(Duration::from_secs(5));
    q.push("b");
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    lease.learn(Duration::from_secs(1));
    settle().await;
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired),
        2,
        "a lease learnt shorter than an entry has waited gives it up at once"
    );
    assert!(sent(&limiter).is_empty(), "a held queue sends nothing");
}

#[tokio::test(start_paused = true)]
async fn a_push_expires_what_waited_one_lease_without_the_drainer() {
    let limiter = Arc::new(Scripted::default());
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(20)), cap: 10 };
    let q = ReleaseQueue::new(limiter, config, metrics.clone());
    q.push("a");
    tokio::time::advance(Duration::from_secs(20)).await;
    q.push("b");
    assert_eq!(q.waiting_keys(), ["b"]);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired), 1);
}

#[tokio::test(start_paused = true)]
async fn a_send_carries_at_most_one_batch() {
    let limiter = Arc::new(Scripted::default());
    let (q, _metrics) = queue(limiter.clone(), 10 * MAX_BATCH);
    q.hold();
    for i in 0..MAX_BATCH + 1 {
        q.push(&format!("k{i}"));
    }
    q.resume();
    settle().await;
    let sizes: Vec<usize> = sent(&limiter).iter().map(Vec::len).collect();
    assert_eq!(sizes, [MAX_BATCH, 1]);
}

#[tokio::test(start_paused = true)]
async fn a_flush_of_an_empty_queue_returns_at_once() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 10);
    let out = q.flush(Duration::from_secs(3)).await;
    assert_eq!(out, ReleaseFlush::default());
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 0);
}

/// A queue whose limiter failed six sends in a row: the next send waits
/// the longest backoff.
async fn backed_off(limiter: &Arc<Scripted>) -> (Arc<ReleaseQueue>, B2buaMetrics) {
    limiter.down.store(true, Ordering::SeqCst);
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(120)), cap: 10 };
    let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
    tokio::spawn(q.clone().run());
    q.push("a");
    for _ in 0..6 {
        tokio::time::advance(BACKOFF_MAX).await;
        settle().await;
    }
    (q, metrics)
}

#[tokio::test(start_paused = true)]
async fn a_flush_sends_at_once_whatever_the_backoff() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = backed_off(&limiter).await;
    let sends = sent(&limiter).len();
    limiter.down.store(false, Ordering::SeqCst);
    q.push("b");
    let out = q.flush(BACKOFF_MAX / 2).await;
    assert_eq!(out, ReleaseFlush { queued: 2, given_up: 0, elapsed: Duration::ZERO });
    assert_eq!(sent(&limiter)[sends..], [keys(&["a", "b"])], "one send, at once");
    assert_eq!(q.waiting(), 0);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 0);
}

#[tokio::test(start_paused = true)]
async fn a_send_failing_during_a_flush_retries_after_the_first_backoff_step() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = backed_off(&limiter).await;
    let sends = sent(&limiter).len();
    let back = limiter.clone();
    let (out, ()) = tokio::join!(q.flush(Duration::from_secs(1)), async move {
        tokio::time::sleep(BACKOFF_INITIAL / 2).await;
        back.down.store(false, Ordering::SeqCst);
    });
    assert_eq!(
        out,
        ReleaseFlush { queued: 1, given_up: 0, elapsed: BACKOFF_INITIAL },
        "the flush's first failed send backs off one step, not the longest backoff"
    );
    assert_eq!(sent(&limiter)[sends..], [keys(&["a"]), keys(&["a"])]);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 0);
}

#[tokio::test(start_paused = true)]
async fn a_held_queue_is_given_up_at_once_by_a_flush() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 10);
    q.hold();
    q.push("a");
    q.push("b");
    let out = q.flush(Duration::from_secs(3)).await;
    assert_eq!(out, ReleaseFlush { queued: 2, given_up: 2, elapsed: Duration::ZERO });
    assert!(sent(&limiter).is_empty(), "a held queue sends nothing");
    assert_eq!(q.waiting(), 0);
    assert_eq!(metrics.limiter().release_queue_depth(), 0);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 2);
    q.resume();
    settle().await;
    assert!(sent(&limiter).is_empty(), "a given-up entry is never sent");
}

#[tokio::test(start_paused = true)]
async fn a_batch_unanswered_at_the_flush_bound_is_given_up() {
    let limiter = Arc::new(Gated::default());
    let metrics = B2buaMetrics::new();
    let config =
        ReleaseQueueConfig { lease: LimiterLease::starting_at(Duration::from_secs(20)), cap: 10 };
    let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
    tokio::spawn(q.clone().run());
    q.push("a");
    settle().await;
    assert!(q.sending());
    let out = q.flush(Duration::ZERO).await;
    assert_eq!(out, ReleaseFlush { queued: 1, given_up: 1, elapsed: Duration::ZERO });
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 1);
    limiter.go.notify_one();
    settle().await;
    assert_eq!(q.waiting(), 0, "the late answer finds nothing to remove");
}

#[tokio::test(start_paused = true)]
async fn giving_up_every_entry_counts_them_as_a_shutdown_drop() {
    let limiter = Arc::new(Scripted::default());
    let (q, metrics) = queue(limiter.clone(), 10);
    q.hold();
    q.push("a");
    q.push("b");
    assert_eq!(q.give_up_all(), 2);
    assert_eq!(q.waiting(), 0);
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 2);
    assert_eq!(q.give_up_all(), 0, "nothing left to give up");
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 2);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_queue_has_nothing_to_flush() {
    let limiter = Arc::new(Scripted::default());
    limiter.down.store(true, Ordering::SeqCst);
    let (q, metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    q.stop();
    let out = q.flush(Duration::from_secs(3)).await;
    assert_eq!(out, ReleaseFlush::default(), "a crashed worker's queue is lost, not flushed");
    assert_eq!(
        metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown),
        0,
        "a crash counts nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stop_ends_a_waiting_flush_at_once() {
    let limiter = Arc::new(Scripted::default());
    limiter.down.store(true, Ordering::SeqCst);
    let (q, metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    let stopper = q.clone();
    let (out, ()) = tokio::join!(q.flush(Duration::from_secs(3)), async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.stop();
    });
    assert_eq!(out, ReleaseFlush::default(), "the crash cut the flush, not its bound");
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 0);
}

#[tokio::test(start_paused = true)]
async fn a_breaker_opening_during_a_flush_gives_the_queue_up_at_once() {
    let limiter = Arc::new(Scripted::default());
    limiter.down.store(true, Ordering::SeqCst);
    let (q, metrics) = queue(limiter.clone(), 10);
    q.push("a");
    settle().await;
    let breaker = q.clone();
    let (out, ()) = tokio::join!(q.flush(Duration::from_secs(3)), async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        breaker.hold();
    });
    assert_eq!(out, ReleaseFlush { queued: 1, given_up: 1, elapsed: Duration::from_millis(100) });
    assert_eq!(metrics.limiter().release_given_up_total(ReleaseGiveUp::Shutdown), 1);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_queue_has_nothing_unsent() {
    let limiter = Arc::new(Scripted::default());
    let (q, _metrics) = queue(limiter.clone(), 10);
    q.hold();
    q.push("a");
    assert_eq!(q.unsent(), 1);
    q.stop();
    assert_eq!(q.unsent(), 0, "a crashed worker's queue is lost");
}
