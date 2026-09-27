//! [`ReleaseQueue`] — the worker's queue of limiter releases.
//!
//! Every release of a call's limiter key is handed to the queue and the caller
//! moves on, so a call writes its CDR and is removed in its last turn whatever
//! the limiter does. One drainer ([`ReleaseQueue::run`]) sends: at once when
//! nothing is in flight, every waiting key in one request (up to
//! [`MAX_BATCH`] keys), under the client's release budget. A failed send keeps
//! its keys, and the next send waits a backoff that doubles with each
//! consecutive failure; the limiter is one endpoint, so the backoff is the
//! queue's, not the entry's, and the keys stay in one batch. A drainer that
//! panics is restarted with the queue intact, and counted.
//!
//! Bounds: an entry that has waited one lease (at most [`MAX_LEASE`]) is
//! given up (the limiter already let the call's set lapse), and a push onto a
//! full queue gives up the oldest entry; both are counted. An entry is a key
//! and its lease expiry, nothing of the call. The queue is not replicated: a
//! worker that dies loses it, and the lease frees what it held; a release is
//! idempotent per key, so a key another node also releases frees nothing
//! twice.
//!
//! [`ReleaseQueue::hold`] and [`ReleaseQueue::resume`] are the seam a circuit
//! breaker drives: while held nothing is sent (leases still expire), and a
//! resume sends every waiting key at once.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::abort_on_drop::AbortOnDrop;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::config::B2buaConfig;
use crate::limiter::{CallLimiter, ReleaseAnswer};
use crate::metrics::B2buaMetrics;

/// Most keys one release request carries.
pub const MAX_BATCH: usize = 1_000;

/// The wait before the first resend after a failed send.
pub const BACKOFF_INITIAL: Duration = Duration::from_millis(200);

/// The longest wait between two sends while the limiter keeps failing.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The longest lease the queue honours.
pub const MAX_LEASE: Duration = Duration::from_secs(B2buaConfig::MAX_LIMITER_LEASE_SEC as u64);

/// The queue's bounds.
#[derive(Clone, Copy, Debug)]
pub struct ReleaseQueueConfig {
    /// How long an entry is worth sending: the limiter's lease.
    pub lease: Duration,
    /// Most entries the queue holds.
    pub cap: usize,
}

impl ReleaseQueueConfig {
    /// The bounds `config` states, the lease clamped to [`MAX_LEASE`].
    pub fn from_config(config: &B2buaConfig) -> Self {
        Self {
            lease: Duration::from_secs(config.limiter_lease_sec.max(0) as u64).min(MAX_LEASE),
            cap: config.limiter_release_queue_cap.max(1),
        }
    }
}

/// One waiting release.
struct Entry {
    /// The key, shared with [`Waiting::by_key`].
    key: Arc<str>,
    /// Past this instant the limiter has let the call's set lapse.
    lease_expires_at: Instant,
}

#[derive(Default)]
struct Waiting {
    /// Entries by push order: the first is the oldest and expires first.
    entries: BTreeMap<u64, Entry>,
    /// The push order of every waiting key.
    by_key: HashMap<Arc<str>, u64>,
    next_seq: u64,
    /// Consecutive failed sends.
    failures: u32,
    /// No send before this instant (set by a failed send).
    retry_at: Option<Instant>,
    /// A breaker holds the queue: nothing is sent.
    held: bool,
    /// A release request is in flight.
    sending: bool,
}

/// What the drainer does next.
enum Step {
    /// Send these keys, pushed under these sequence numbers.
    Send(Vec<u64>, Vec<String>),
    /// Nothing to send before this instant (or before a wake, when `None`).
    Wait(Option<Instant>),
}

/// The worker's queue of limiter releases. See the module doc.
pub struct ReleaseQueue {
    limiter: Arc<dyn CallLimiter>,
    config: ReleaseQueueConfig,
    metrics: B2buaMetrics,
    waiting: Mutex<Waiting>,
    wake: Notify,
}

impl ReleaseQueue {
    /// An empty queue sending through `limiter`. Nothing is sent until
    /// [`run`](Self::run) is spawned.
    pub fn new(
        limiter: Arc<dyn CallLimiter>,
        config: ReleaseQueueConfig,
        metrics: B2buaMetrics,
    ) -> Arc<Self> {
        Arc::new(Self {
            limiter,
            config,
            metrics,
            waiting: Mutex::new(Waiting::default()),
            wake: Notify::new(),
        })
    }

    /// The queue's state. A panic under the lock leaves the state whole
    /// (every step keeps `entries` and `by_key` in step or loses a key the
    /// lease frees), so a poisoned lock is taken as it is.
    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue the release of `key` and return at once. A key already waiting
    /// stays one entry; on a full queue the oldest entry is given up.
    pub fn push(&self, key: &str) {
        let now = Instant::now();
        let mut w = self.lock();
        self.expire(&mut w, now);
        if w.by_key.contains_key(key) {
            return;
        }
        while w.entries.len() >= self.config.cap {
            let Some((_, oldest)) = w.entries.pop_first() else { break };
            w.by_key.remove(&oldest.key);
            self.metrics.bump_limiter_release_dropped_cap();
        }
        let seq = w.next_seq;
        w.next_seq += 1;
        let key: Arc<str> = Arc::from(key);
        let lease_expires_at = now + self.config.lease;
        w.entries.insert(seq, Entry { key: key.clone(), lease_expires_at });
        w.by_key.insert(key, seq);
        self.publish_depth(&w);
        drop(w);
        self.wake.notify_one();
    }

    /// Releases waiting, the batch in flight included.
    pub fn waiting(&self) -> usize {
        self.lock().entries.len()
    }

    /// The waiting keys, oldest first.
    pub fn waiting_keys(&self) -> Vec<String> {
        self.lock().entries.values().map(|e| e.key.to_string()).collect()
    }

    /// A release request is in flight (at most one at a time).
    pub fn sending(&self) -> bool {
        self.lock().sending
    }

    /// Stop sending (a breaker opened): entries wait, and still expire.
    pub fn hold(&self) {
        self.lock().held = true;
    }

    /// Send again (a breaker closed): every waiting key leaves at once,
    /// whatever backoff a failed send had set.
    pub fn resume(&self) {
        {
            let mut w = self.lock();
            w.held = false;
            w.failures = 0;
            w.retry_at = None;
        }
        self.wake.notify_one();
    }

    /// The supervised drainer, until the task is aborted with the worker. A
    /// drainer that panics is logged, counted and started again; the panic
    /// counts as a failed send, so what it was sending is still queued and
    /// leaves with the next send, one backoff later.
    pub async fn run(self: Arc<Self>) {
        loop {
            let mut drainer = AbortOnDrop(tokio::spawn(self.clone().drain()));
            match (&mut drainer.0).await {
                Err(e) if e.is_panic() => {
                    {
                        let mut w = self.lock();
                        w.sending = false;
                        self.back_off(&mut w, Instant::now());
                    }
                    self.metrics.bump_limiter_release_drainer_restarts();
                    tracing::error!(
                        waiting = self.waiting(),
                        "limiter release drainer panicked; restarting it"
                    );
                }
                _ => return,
            }
        }
    }

    /// Send what is due, then wait for the next due instant or a push.
    async fn drain(self: Arc<Self>) {
        loop {
            match self.step(Instant::now()) {
                Step::Send(seqs, keys) => {
                    let outcome = self.limiter.release(&keys).await;
                    self.settle(&seqs, outcome, Instant::now());
                }
                Step::Wait(Some(at)) => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                Step::Wait(None) => self.wake.notified().await,
            }
        }
    }

    /// Give up the entries past their lease, then say what to send, or until
    /// when to wait.
    fn step(&self, now: Instant) -> Step {
        let mut w = self.lock();
        self.expire(&mut w, now);
        let Some(oldest) = w.entries.first_key_value().map(|(_, e)| e.lease_expires_at) else {
            return Step::Wait(None);
        };
        if w.held {
            return Step::Wait(Some(oldest));
        }
        if let Some(at) = w.retry_at.filter(|at| *at > now) {
            return Step::Wait(Some(at.min(oldest)));
        }
        let (seqs, keys) =
            w.entries.iter().take(MAX_BATCH).map(|(seq, e)| (*seq, e.key.to_string())).unzip();
        w.sending = true;
        Step::Send(seqs, keys)
    }

    /// Apply a send's outcome: an answered send removes its keys and clears
    /// the backoff; a failed one keeps them and backs off.
    fn settle(&self, seqs: &[u64], outcome: ReleaseAnswer, now: Instant) {
        let mut w = self.lock();
        w.sending = false;
        match outcome {
            ReleaseAnswer::Released => {
                for seq in seqs {
                    if let Some(entry) = w.entries.remove(seq) {
                        w.by_key.remove(&entry.key);
                    }
                }
                w.failures = 0;
                w.retry_at = None;
            }
            ReleaseAnswer::Unavailable => {
                self.back_off(&mut w, now);
                let kept = seqs.iter().filter(|seq| w.entries.contains_key(seq)).count();
                self.metrics.add_limiter_release_retries(kept as u64);
            }
        }
        self.publish_depth(&w);
    }

    /// One more failed send: no send before the next backoff step.
    fn back_off(&self, w: &mut Waiting, now: Instant) {
        w.failures = w.failures.saturating_add(1);
        w.retry_at = Some(now + backoff(w.failures));
    }

    /// Give up every entry that has waited one lease.
    fn expire(&self, w: &mut Waiting, now: Instant) {
        let mut dropped = false;
        while let Some(entry) = w.entries.first_entry() {
            if entry.get().lease_expires_at > now {
                break;
            }
            let entry = entry.remove();
            w.by_key.remove(&entry.key);
            self.metrics.bump_limiter_release_dropped_lease_expired();
            dropped = true;
        }
        if dropped {
            self.publish_depth(w);
        }
    }

    fn publish_depth(&self, w: &Waiting) {
        self.metrics.set_limiter_release_queue_depth(w.entries.len() as u64);
    }
}

/// The wait after `failures` consecutive failed sends.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    BACKOFF_INITIAL.saturating_mul(1 << doublings).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::limiter::{AdmitOutcome, LimiterEntry, RefreshOutcome};

    /// A limiter whose releases answer or fail at the test's say, logging
    /// the keys of every release request.
    #[derive(Default)]
    struct Scripted {
        down: AtomicBool,
        sent: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl CallLimiter for Scripted {
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
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
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            RefreshOutcome::Unavailable
        }
    }

    fn queue(limiter: Arc<Scripted>, cap: usize) -> (Arc<ReleaseQueue>, B2buaMetrics) {
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(20), cap };
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
        assert_eq!(metrics.limiter_release_queue_depth(), 0);
    }

    /// Answers a release only once the test lets it go.
    #[derive(Default)]
    struct Gated {
        go: tokio::sync::Notify,
    }

    #[async_trait]
    impl CallLimiter for Gated {
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
            AdmitOutcome::NotSent
        }
        async fn release(&self, _: &[String]) -> ReleaseAnswer {
            self.go.notified().await;
            ReleaseAnswer::Released
        }
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            RefreshOutcome::Unavailable
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_queue_says_while_its_request_is_in_flight() {
        let limiter = Arc::new(Gated::default());
        let q = ReleaseQueue::new(
            limiter.clone(),
            ReleaseQueueConfig { lease: Duration::from_secs(20), cap: 10 },
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
        assert_eq!(metrics.limiter_release_retries_total(), 1);

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
        assert_eq!(metrics.limiter_release_dropped_lease_expired_total(), 1);
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
        assert_eq!(metrics.limiter_release_dropped_cap_total(), 1);
        assert_eq!(metrics.limiter_release_queue_depth(), 2);
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
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
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
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            RefreshOutcome::Unavailable
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_drainer_is_restarted_with_the_queue_intact() {
        let limiter = Arc::new(PanicsOnce::default());
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(20), cap: 10 };
        let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
        tokio::spawn(q.clone().run());
        q.push("a");
        settle().await;
        q.push("b");
        settle().await;
        assert_eq!(metrics.limiter_release_drainer_restarts_total(), 1);
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
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
            AdmitOutcome::NotSent
        }
        async fn release(&self, keys: &[String]) -> ReleaseAnswer {
            assert!(!self.panics.load(Ordering::SeqCst), "the limiter client panics");
            self.answered.lock().unwrap().push(keys.to_vec());
            ReleaseAnswer::Released
        }
        async fn refresh(&self, _: &str, _: &[String]) -> RefreshOutcome {
            RefreshOutcome::Unavailable
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_sender_that_keeps_panicking_restarts_at_the_backoff_pace() {
        let limiter = Arc::new(PanicsWhileSet::default());
        limiter.panics.store(true, Ordering::SeqCst);
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(120), cap: 10 };
        let q = ReleaseQueue::new(limiter.clone(), config, metrics.clone());
        tokio::spawn(q.clone().run());
        q.push("a");
        for _ in 0..600 {
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
        let restarts = metrics.limiter_release_drainer_restarts_total();
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
    async fn a_lease_past_the_bound_is_clamped() {
        let config = B2buaConfig { limiter_lease_sec: i64::MAX, ..Default::default() };
        let bounds = ReleaseQueueConfig::from_config(&config);
        assert_eq!(bounds.lease, MAX_LEASE);
        let limiter = Arc::new(Scripted::default());
        let q = ReleaseQueue::new(limiter.clone(), bounds, B2buaMetrics::new());
        q.push("a");
        assert_eq!(q.waiting(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_push_expires_what_waited_one_lease_without_the_drainer() {
        let limiter = Arc::new(Scripted::default());
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig { lease: Duration::from_secs(20), cap: 10 };
        let q = ReleaseQueue::new(limiter, config, metrics.clone());
        q.push("a");
        tokio::time::advance(Duration::from_secs(20)).await;
        q.push("b");
        assert_eq!(q.waiting_keys(), ["b"]);
        assert_eq!(metrics.limiter_release_dropped_lease_expired_total(), 1);
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
}
