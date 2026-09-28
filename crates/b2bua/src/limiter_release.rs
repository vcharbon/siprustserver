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
//! Bounds: an entry that has waited one lease is given up (the limiter
//! already let the call's set lapse), and a push onto a full queue gives up
//! the oldest entry; both are counted. The lease is the limiter's as the
//! worker last learnt it ([`LimiterLease`]): a lease learnt later moves every
//! waiting entry's deadline, and the drainer wakes on it, so an entry is given
//! up at its deadline under the lease of the moment, held queue included. An
//! entry is a key and the instant it was queued, nothing of the call. The
//! queue is not replicated: a worker that dies loses it, and the lease frees
//! what it held; a release is idempotent per key, so a key another node also
//! releases frees nothing twice.
//!
//! [`ReleaseQueue::hold`] and [`ReleaseQueue::resume`] are the seam a circuit
//! breaker drives: while held nothing is sent (leases still expire), and a
//! resume sends every waiting key at once. [`ReleaseQueue::on_push`] tells
//! the refresh batch which calls ended, so it forgets their refreshes.
//!
//! [`ReleaseQueue::flush`] is a planned exit's last send: every waiting key
//! leaves at once and the exit waits, within a bound, for the queue to
//! empty; what is still queued at the bound (a held queue's entries, a batch
//! the limiter has not answered) is given up, counted and logged. A crash
//! loses the queue unflushed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crate::abort_on_drop::AbortOnDrop;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::config::B2buaConfig;
use crate::limiter::{CallLimiter, ReleaseAnswer};
use crate::limiter_lease::LimiterLease;
use crate::metrics::{B2buaMetrics, LimiterTask, ReleaseGiveUp};

/// Most keys one release request carries.
pub const MAX_BATCH: usize = 1_000;

/// The wait before the first resend after a failed send.
pub const BACKOFF_INITIAL: Duration = Duration::from_millis(200);

/// The longest wait between two sends while the limiter keeps failing.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The queue's bounds.
#[derive(Clone)]
pub struct ReleaseQueueConfig {
    /// How long an entry is worth sending: the limiter's lease as last
    /// learnt.
    pub lease: Arc<LimiterLease>,
    /// Most entries the queue holds.
    pub cap: usize,
}

impl ReleaseQueueConfig {
    /// The cap `config` states, and the worker's learnt `lease`.
    pub fn from_config(config: &B2buaConfig, lease: Arc<LimiterLease>) -> Self {
        Self { lease, cap: config.limiter_release_queue_cap.max(1) }
    }
}

/// What a [`ReleaseQueue::flush`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReleaseFlush {
    /// Entries waiting when the flush began.
    pub queued: usize,
    /// Entries still queued at the bound, given up (a batch in flight
    /// included: it may still land).
    pub given_up: usize,
    /// How long the flush waited.
    pub elapsed: Duration,
}

impl ReleaseFlush {
    /// What the flush did, from its counts.
    pub fn outcome(&self) -> ReleaseFlushOutcome {
        match (self.queued, self.given_up) {
            (_, 1..) => ReleaseFlushOutcome::GivenUp,
            (0, 0) => ReleaseFlushOutcome::Empty,
            _ => ReleaseFlushOutcome::Sent,
        }
    }
}

/// What a [`ReleaseQueue::flush`] did, as the `outcome` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseFlushOutcome {
    /// Nothing was queued.
    Empty,
    /// Every queued release was answered.
    Sent,
    /// Some queued releases were given up at the bound.
    GivenUp,
}

impl ReleaseFlushOutcome {
    /// Every outcome, in label order.
    pub const ALL: [ReleaseFlushOutcome; 3] =
        [ReleaseFlushOutcome::Empty, ReleaseFlushOutcome::Sent, ReleaseFlushOutcome::GivenUp];

    /// The metric and log label.
    pub fn label(self) -> &'static str {
        match self {
            ReleaseFlushOutcome::Empty => "empty",
            ReleaseFlushOutcome::Sent => "sent",
            ReleaseFlushOutcome::GivenUp => "given_up",
        }
    }
}

/// One waiting release.
struct Entry {
    /// The key, shared with [`Waiting::by_key`].
    key: Arc<str>,
    /// When it was queued: one lease later the limiter has let the call's
    /// set lapse.
    queued_at: Instant,
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
    /// The worker crashed: the queue is lost, nothing is flushed.
    stopped: bool,
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
    /// Woken each time the queue is left empty, is held or is stopped.
    settled: Notify,
    /// Told every key pushed.
    on_push: OnceLock<PushHook>,
}

/// What [`ReleaseQueue::on_push`] registers.
type PushHook = Box<dyn Fn(&str) + Send + Sync>;

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
            settled: Notify::new(),
            on_push: OnceLock::new(),
        })
    }

    /// Tell `hook` every key pushed from now on, before it is queued. One
    /// hook per queue: a second registration is refused (`false`).
    pub fn on_push(&self, hook: impl Fn(&str) + Send + Sync + 'static) -> bool {
        self.on_push.set(Box::new(hook)).is_ok()
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
        if let Some(hook) = self.on_push.get() {
            hook(key);
        }
        let now = Instant::now();
        let mut w = self.lock();
        self.expire(&mut w, now);
        if w.by_key.contains_key(key) {
            return;
        }
        while w.entries.len() >= self.config.cap {
            let Some((_, oldest)) = w.entries.pop_first() else { break };
            w.by_key.remove(&oldest.key);
            self.metrics.limiter().count_release_given_up(ReleaseGiveUp::Cap, 1);
        }
        let seq = w.next_seq;
        w.next_seq += 1;
        let key: Arc<str> = Arc::from(key);
        w.entries.insert(seq, Entry { key: key.clone(), queued_at: now });
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

    /// Releases a flush still has to send: the waiting entries, none once
    /// the queue is stopped.
    pub fn unsent(&self) -> usize {
        let w = self.lock();
        if w.stopped {
            0
        } else {
            w.entries.len()
        }
    }

    /// A release request is in flight (at most one at a time).
    pub fn sending(&self) -> bool {
        self.lock().sending
    }

    /// Stop sending (a breaker opened): entries wait, and still expire.
    pub fn hold(&self) {
        self.lock().held = true;
        self.settled.notify_waiters();
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

    /// A planned exit's last send: every waiting key leaves now, whatever
    /// backoff a failed send had set (a send failing during the flush backs
    /// off from the first step), and the flush returns once the queue is
    /// empty or `within` has passed. A held queue (an open breaker, now or
    /// during the flush) is given up at once. Every entry still queued at the
    /// bound is given up, counted and logged. A key pushed during the flush
    /// is flushed with it; one pushed after it returns is sent only if the
    /// worker lives on, and is otherwise lost uncounted with the process
    /// (a drain flushes again until its queue reads empty). A stopped queue
    /// has nothing to flush.
    pub async fn flush(&self, within: Duration) -> ReleaseFlush {
        let start = Instant::now();
        let deadline = start + within;
        let queued = {
            let mut w = self.lock();
            if w.stopped {
                return ReleaseFlush::default();
            }
            w.failures = 0;
            w.retry_at = None;
            w.entries.len()
        };
        self.wake.notify_one();
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let (empty, held, stopped) = {
                let w = self.lock();
                (w.entries.is_empty(), w.held, w.stopped)
            };
            if stopped {
                return ReleaseFlush::default();
            }
            if held && !empty {
                let given_up = self.give_up_all();
                return ReleaseFlush { queued, given_up, elapsed: start.elapsed() };
            }
            if empty {
                let elapsed = start.elapsed();
                if queued > 0 {
                    tracing::info!(
                        queued,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "limiter release queue flushed"
                    );
                }
                return ReleaseFlush { queued, given_up: 0, elapsed };
            }
            tokio::select! {
                _ = settled => {}
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let given_up = self.give_up_all();
        ReleaseFlush { queued, given_up, elapsed: start.elapsed() }
    }

    /// Give up every waiting entry, a batch in flight included (it may still
    /// land): a planned exit that sends no more. The entries are counted
    /// (`reason=shutdown`) and logged; returns how many.
    pub fn give_up_all(&self) -> usize {
        let (given_up, held) = {
            let mut w = self.lock();
            let given_up = w.entries.len();
            w.entries.clear();
            w.by_key.clear();
            self.publish_depth(&w);
            (given_up, w.held)
        };
        if given_up > 0 {
            self.metrics.limiter().count_release_given_up(ReleaseGiveUp::Shutdown, given_up as u64);
            tracing::warn!(
                given_up,
                held,
                "limiter releases given up at exit; the lease frees them"
            );
        }
        given_up
    }

    /// Whether the worker crashed ([`stop`](Self::stop)).
    pub fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    /// The worker crashed: its queue is lost as it stands, and a flush
    /// returns at once.
    pub fn stop(&self) {
        self.lock().stopped = true;
        self.settled.notify_waiters();
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
                    self.metrics.limiter().count_task_restart(LimiterTask::ReleaseSender);
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
    /// A change of the lease moves every deadline: the wait is looked at
    /// again.
    async fn drain(self: Arc<Self>) {
        let mut lease = self.config.lease.changes();
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
                        _ = lease_changed(&mut lease) => {}
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
        let lease = self.config.lease.current();
        let Some(oldest) = w.entries.first_key_value().map(|(_, e)| e.queued_at + lease) else {
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
                self.metrics.limiter().count_release_retries(kept as u64);
            }
        }
        self.publish_depth(&w);
    }

    /// One more failed send: no send before the next backoff step.
    fn back_off(&self, w: &mut Waiting, now: Instant) {
        w.failures = w.failures.saturating_add(1);
        w.retry_at = Some(now + backoff(w.failures));
    }

    /// Give up every entry that has waited one lease. Entries are in queue
    /// order, so the first is the first to expire.
    fn expire(&self, w: &mut Waiting, now: Instant) {
        let lease = self.config.lease.current();
        let mut dropped = false;
        while let Some(entry) = w.entries.first_entry() {
            if entry.get().queued_at + lease > now {
                break;
            }
            let entry = entry.remove();
            w.by_key.remove(&entry.key);
            self.metrics.limiter().count_release_given_up(ReleaseGiveUp::LeaseExpired, 1);
            dropped = true;
        }
        if dropped {
            self.publish_depth(w);
        }
    }

    /// Publish the queue's depth, and wake a flush when it is empty.
    fn publish_depth(&self, w: &Waiting) {
        self.metrics.limiter().set_release_queue_depth(w.entries.len() as u64);
        if w.entries.is_empty() {
            self.settled.notify_waiters();
        }
    }
}

/// Resolves on the next change of the lease; never once its sender is gone.
pub(crate) async fn lease_changed(lease: &mut tokio::sync::watch::Receiver<Duration>) {
    if lease.changed().await.is_err() {
        std::future::pending::<()>().await;
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
    use crate::limiter::{AdmitOutcome, LimiterEntry, RefreshAnswer, RefreshCall};

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
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
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
            ReleaseQueueConfig {
                lease: LimiterLease::starting_at(Duration::from_secs(20)),
                cap: 10,
            },
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
        async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
            RefreshAnswer::Unavailable
        }
        fn report_to(&self, _: crate::limiter::LimiterReports) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_drainer_is_restarted_with_the_queue_intact() {
        let limiter = Arc::new(PanicsOnce::default());
        let metrics = B2buaMetrics::new();
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(20)),
            cap: 10,
        };
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
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
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
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
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
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(20)),
            cap: 10,
        };
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
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(120)),
            cap: 10,
        };
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
        let config = ReleaseQueueConfig {
            lease: LimiterLease::starting_at(Duration::from_secs(20)),
            cap: 10,
        };
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
        assert_eq!(
            out,
            ReleaseFlush { queued: 1, given_up: 1, elapsed: Duration::from_millis(100) }
        );
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
}
