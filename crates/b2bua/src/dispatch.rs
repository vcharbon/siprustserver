//! `PerCallDispatcher` — the per-call FIFO (port of `PerCallDispatcher.ts`,
//! source ADR-0004/0005). Each call gets a bounded queue + a worker task that
//! runs its handler bodies strictly in order; a global semaphore caps total
//! in-flight handlers so a slow handler on one call never blocks other calls.
//!
//! The handler body is a boxed future (the Rust analogue of the source's
//! type-erased `Effect`). Bodies are run on spawned tasks the worker awaits, so
//! a panicking handler is isolated (`JoinError`) and the worker survives.
//!
//! A body offered as a [`Job`] may be discarded unrun — the call's queue full,
//! the global cap reached, or queued behind the call's release — and its
//! [`DiscardHook`] then hears why and answers for it. A job marked
//! [`past_bounds`](Job::past_bounds) is never discarded for want of room.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, Semaphore};

use crate::metrics::{B2buaMetrics, RemovalClass};

/// A unit of per-call work — a self-contained future capturing the router +
/// the event.
pub type DispatchBody = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Why a handler body died (ADR-0020 X6). `Panicked` = the spawned body's
/// `JoinError::is_panic()`; `Aborted` = the call reaper's escalation cancelled
/// a hung body via [`PerCallDispatcher::abort_in_flight`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerFailure {
    Panicked,
    Aborted,
}

/// Notified from the per-call worker when a handler body failed — AFTER the
/// body unwound (its per-call lock guard is already released) and BEFORE the
/// worker dequeues the next item, so notifications are FIFO-ordered with the
/// call's own event stream. Must be cheap and non-blocking (it runs on the
/// worker). The call reaper installs the only production hook (two-strike
/// escalation); `None` keeps the pre-ADR-0020 swallow.
pub type FailureHook = Arc<dyn Fn(&str, HandlerFailure) + Send + Sync>;

/// Why a body was discarded without running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// The call's queue was full.
    QueueFull,
    /// The call had no queue and the global cap was reached.
    AtCap,
    /// It was queued behind the call's release, of this class, and drained
    /// with it.
    Released(RemovalClass),
}

/// Answers for a body discarded unrun: handed the reason, it returns the work
/// to run in the body's place, which the discarding site awaits.
pub type DiscardHook = Box<dyn FnOnce(Discard) -> DispatchBody + Send>;

/// How far past the dispatcher's bounds a job may wait instead of being
/// discarded for want of room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Discarded when the call's queue is full or the global cap is reached.
    Bounded,
    /// Waits past a full queue and past the cap, up to the call's overflow
    /// ceiling.
    PastBounds,
    /// Waits past every bound, the overflow ceiling included.
    Always,
}

/// A body offered to [`PerCallDispatcher::dispatch`], with what becomes of it
/// if it never runs.
pub struct Job {
    body: DispatchBody,
    on_discard: Option<DiscardHook>,
    admission: Admission,
    /// Counted toward the call's lifetime cap; `false` for the node's own work.
    counted: bool,
}

impl Job {
    /// A body discarded when there is no room for it.
    pub fn new(body: DispatchBody) -> Self {
        Self { body, on_discard: None, admission: Admission::Bounded, counted: true }
    }

    /// `hook` runs in place of the body if it is discarded unrun; dropped
    /// unheard once the body starts.
    pub fn on_discard(mut self, hook: Option<DiscardHook>) -> Self {
        self.on_discard = hook;
        self
    }

    /// Queued past a full per-call queue and past the global cap, in FIFO
    /// order, while the call's overflow holds fewer such jobs than its queue
    /// depth. Past that ceiling it is discarded as for a full queue and the
    /// overflow hook hears the call. A release discards it like any job.
    pub fn past_bounds(mut self) -> Self {
        self.admission = Admission::PastBounds;
        self
    }

    /// Queued past every bound, the overflow ceiling included; discarded only
    /// behind the call's release. For an event its producer bounds per call:
    /// a sweep-paced verdict, a timer fired once, a transaction's outcome.
    pub fn past_all_bounds(mut self) -> Self {
        self.admission = Admission::Always;
        self
    }

    /// The node's own work — a timer it armed, an event it raised itself —
    /// which does not count toward the call's lifetime cap.
    pub fn own(mut self) -> Self {
        self.counted = false;
        self
    }

    /// Drop the body unrun and run the hook in its place.
    async fn discard(self, why: Discard) {
        drop(self.body);
        if let Some(hook) = self.on_discard {
            hook(why).await;
        }
    }
}

enum DispatchItem {
    Event(Job),
    Poison(RemovalClass),
}

impl DispatchItem {
    fn admission(&self) -> Admission {
        match self {
            DispatchItem::Event(job) => job.admission,
            DispatchItem::Poison(_) => Admission::Always,
        }
    }
}

/// Why [`PerCallQueue::push`] turned an item away.
enum Refusal {
    /// No room; `ceiling` when a past-bounds job found the overflow full too.
    Full { ceiling: bool },
    /// The call's release is already queued; the item would be drained with
    /// it.
    Released(RemovalClass),
}

/// A call's queue: the bounded channel its worker reads, then the items
/// admitted past it. Every item in `overflow` is younger than every item in
/// the channel: overflow items move to the channel oldest first as it frees,
/// new items join the overflow only while the channel stays full, and the
/// worker takes from the overflow only once the channel is empty.
struct PerCallQueue {
    tx: mpsc::Sender<DispatchItem>,
    overflow: VecDeque<DispatchItem>,
    /// The class of the release queued on this call, once one is: every later
    /// item would be drained behind it.
    released: Option<RemovalClass>,
    /// How many past-bounds jobs wait in `overflow` — what the ceiling
    /// counts. Items admitted past every bound are not among them.
    waiting: usize,
    /// Counted jobs offered for the call over its life, whatever became of
    /// them.
    offered: u64,
    /// The lifetime cap was crossed: only jobs past every bound still get in.
    capped: bool,
}

impl PerCallQueue {
    fn new(tx: mpsc::Sender<DispatchItem>) -> Self {
        Self {
            tx,
            overflow: VecDeque::new(),
            released: None,
            waiting: 0,
            offered: 0,
            capped: false,
        }
    }

    /// Count `job` against the lifetime cap `cap`; `true` when this job is
    /// the one that crosses it.
    fn count(&mut self, job: &Job, cap: u64) -> bool {
        if !job.counted {
            return false;
        }
        self.offered += 1;
        let crossed = self.offered > cap && !self.capped;
        self.capped |= crossed;
        crossed
    }

    /// Queue `item` in FIFO order, or hand it back with why not. `ceiling`
    /// caps the past-bounds jobs the overflow holds.
    fn push(
        &mut self,
        item: DispatchItem,
        ceiling: usize,
        metrics: &B2buaMetrics,
    ) -> Result<(), (DispatchItem, Refusal)> {
        if let Some(class) = self.released {
            return Err((item, Refusal::Released(class)));
        }
        if let DispatchItem::Poison(class) = item {
            self.released = Some(class);
        }
        self.refill(metrics);
        let item = if self.overflow.is_empty() {
            match self.tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(item))
                | Err(mpsc::error::TrySendError::Closed(item)) => item,
            }
        } else {
            item
        };
        match item.admission() {
            Admission::Bounded => return Err((item, Refusal::Full { ceiling: false })),
            Admission::PastBounds if self.waiting >= ceiling => {
                return Err((item, Refusal::Full { ceiling: true }))
            }
            Admission::PastBounds => self.waiting += 1,
            Admission::Always => {}
        }
        metrics.bump_past_bound(PastBound::Depth);
        metrics.add_overflow_depth(1);
        self.overflow.push_back(item);
        Ok(())
    }

    /// Take the overflow's oldest item, keeping its counts.
    fn pop_overflow(&mut self, metrics: &B2buaMetrics) -> Option<DispatchItem> {
        let item = self.overflow.pop_front()?;
        left_overflow(&mut self.waiting, &item, metrics);
        Some(item)
    }

    /// Move overflow items into the channel's free slots, oldest first.
    fn refill(&mut self, metrics: &B2buaMetrics) {
        while !self.overflow.is_empty() {
            let Ok(slot) = self.tx.try_reserve() else { return };
            let front = self.overflow.pop_front().expect("the overflow is not empty");
            left_overflow(&mut self.waiting, &front, metrics);
            slot.send(front);
        }
    }
}

/// Account for `item` leaving a call's overflow.
fn left_overflow(waiting: &mut usize, item: &DispatchItem, metrics: &B2buaMetrics) {
    metrics.add_overflow_depth(-1);
    if item.admission() == Admission::PastBounds {
        *waiting -= 1;
    }
}

/// Which bound a past-bounds item was queued past (metric label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastBound {
    /// The call's queue was full.
    Depth,
    /// The global queue cap was reached.
    Cap,
}

/// Hears a call whose overflow ceiling turned a past-bounds job away: more
/// must-run events are arriving than the call ever consumes. The call reaper
/// installs the only production hook (it tears the call down).
pub type OverflowHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Hears a call that crossed its lifetime cap, once. The call reaper installs
/// the only production hook (it ends the call).
pub type LifetimeHook = Arc<dyn Fn(&str) + Send + Sync>;

/// What [`PerCallDispatcher::enqueue`] did with a job.
enum Enqueued {
    Queued,
    Discarded(Discard, Job),
    /// Discarded at the overflow ceiling.
    Overflowed(Job),
}

type QueueMap = Arc<Mutex<HashMap<String, PerCallQueue>>>;
type InflightMap = Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>;

/// The dispatcher handle. Clone-cheap.
#[derive(Clone)]
pub struct PerCallDispatcher {
    queues: QueueMap,
    semaphore: Arc<Semaphore>,
    depth: usize,
    cap: usize,
    metrics: B2buaMetrics,
    failure_hook: Option<FailureHook>,
    overflow_hook: Option<OverflowHook>,
    /// Counted jobs a call may be offered over its life, and who hears the
    /// call that crosses it.
    lifetime_cap: u64,
    lifetime_hook: Option<LifetimeHook>,
    /// The currently in-flight body per call (FIFO ⇒ at most one) — the
    /// reaper's [`abort_in_flight`](Self::abort_in_flight) target.
    inflight: InflightMap,
}

impl PerCallDispatcher {
    pub fn new(concurrency: usize, depth: usize, cap: usize, metrics: B2buaMetrics) -> Self {
        Self {
            queues: Arc::new(Mutex::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(concurrency.max(1))),
            depth: depth.max(1),
            cap: cap.max(1),
            metrics,
            failure_hook: None,
            overflow_hook: None,
            lifetime_cap: u64::MAX,
            lifetime_hook: None,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Install the handler-failure hook (builder-style; the call reaper's
    /// two-strike escalation — ADR-0020 X6).
    pub fn with_failure_hook(mut self, hook: FailureHook) -> Self {
        self.failure_hook = Some(hook);
        self
    }

    /// Install the overflow-ceiling hook (builder-style; the call reaper's
    /// teardown of a call flooded past its overflow).
    pub fn with_overflow_hook(mut self, hook: OverflowHook) -> Self {
        self.overflow_hook = Some(hook);
        self
    }

    /// Bound the counted jobs a call may be offered over its life (builder-
    /// style). The job that crosses `cap`, and every later one not admitted
    /// past every bound, is discarded as behind the call's release; `hook`
    /// hears the call once, when it crosses.
    pub fn with_lifetime_cap(mut self, cap: u64, hook: LifetimeHook) -> Self {
        self.lifetime_cap = cap;
        self.lifetime_hook = Some(hook);
        self
    }

    /// Abort the currently in-flight handler body for `call_ref` (ADR-0020 X6
    /// escalation rung): a hung body holds both the worker and the per-call
    /// lock; aborting drops the body future — releasing the lock guard — and
    /// the worker observes the cancellation and reports
    /// [`HandlerFailure::Aborted`]. No-op when the call has no body in flight;
    /// `true` when there was one.
    pub fn abort_in_flight(&self, call_ref: &str) -> bool {
        match self.inflight.lock().unwrap().get(call_ref) {
            Some(h) => {
                h.abort();
                true
            }
            None => false,
        }
    }

    /// Enqueue `job` for `call_ref`, lazily creating the queue + worker. A job
    /// finding no room — the per-call queue full, the global cap reached, the
    /// call's release already queued — is discarded unrun and counted, and its
    /// hook is awaited here.
    pub async fn dispatch(&self, call_ref: &str, job: Job) {
        let (enqueued, crossed) = self.enqueue(call_ref, job);
        if crossed {
            self.metrics.bump_message_cap_lifetime_terminated();
            if let Some(hook) = &self.lifetime_hook {
                hook(call_ref);
            }
        }
        match enqueued {
            Enqueued::Queued => {}
            Enqueued::Discarded(why, job) => job.discard(why).await,
            Enqueued::Overflowed(job) => {
                if let Some(hook) = &self.overflow_hook {
                    hook(call_ref);
                }
                job.discard(Discard::QueueFull).await;
            }
        }
    }

    /// The synchronous half of [`dispatch`](Self::dispatch), under the map
    /// lock, and whether this job crossed the call's lifetime cap.
    fn enqueue(&self, call_ref: &str, job: Job) -> (Enqueued, bool) {
        let mut map = self.queues.lock().unwrap();
        if let Some(q) = map.get_mut(call_ref) {
            let crossed = q.count(&job, self.lifetime_cap);
            if q.capped && job.admission != Admission::Always {
                self.metrics.bump_release_discard();
                let why = Discard::Released(RemovalClass::Terminated);
                return (Enqueued::Discarded(why, job), crossed);
            }
            let enqueued = match q.push(DispatchItem::Event(job), self.depth, &self.metrics) {
                Ok(()) => Enqueued::Queued,
                Err((DispatchItem::Event(job), Refusal::Full { ceiling: false })) => {
                    self.metrics.bump_queue_drop();
                    Enqueued::Discarded(Discard::QueueFull, job)
                }
                Err((DispatchItem::Event(job), Refusal::Full { ceiling: true })) => {
                    self.metrics.bump_queue_drop();
                    self.metrics.bump_overflow_refused();
                    Enqueued::Overflowed(job)
                }
                Err((DispatchItem::Event(job), Refusal::Released(class))) => {
                    self.metrics.bump_release_discard();
                    Enqueued::Discarded(Discard::Released(class), job)
                }
                Err((DispatchItem::Poison(_), _)) => unreachable!("an event was pushed"),
            };
            return (enqueued, crossed);
        }
        if map.len() >= self.cap {
            if job.admission == Admission::Bounded {
                self.metrics.bump_cap_drop();
                return (Enqueued::Discarded(Discard::AtCap, job), false);
            }
            self.metrics.bump_past_bound(PastBound::Cap);
        }
        let (tx, rx) = mpsc::channel(self.depth);
        let mut queue = PerCallQueue::new(tx);
        queue.count(&job, self.lifetime_cap);
        // Send before spawning the worker: capacity is fresh so this can't fail.
        let _ = queue.tx.try_send(DispatchItem::Event(job));
        map.insert(call_ref.to_string(), queue);
        self.metrics.bump_creation();
        tokio::spawn(worker(
            call_ref.to_string(),
            rx,
            self.queues.clone(),
            self.semaphore.clone(),
            self.metrics.clone(),
            self.failure_hook.clone(),
            self.inflight.clone(),
        ));
        (Enqueued::Queued, false)
    }

    /// Signal the worker for `call_ref` to drain and exit (call eviction),
    /// after every item already queued — a full queue included. The first
    /// release names the removal's class; every later item, a second release
    /// included, is discarded as behind it.
    pub fn enqueue_poison(&self, call_ref: &str, class: RemovalClass) {
        let mut map = self.queues.lock().unwrap();
        if let Some(q) = map.get_mut(call_ref) {
            let _ = q.push(DispatchItem::Poison(class), self.depth, &self.metrics);
        }
    }

    pub fn has_queue(&self, call_ref: &str) -> bool {
        self.queues.lock().unwrap().contains_key(call_ref)
    }

    /// Would [`dispatch`](Self::dispatch) cap-drop a bounded job for a
    /// **brand-new** call_ref right now? True iff no queue exists for it AND the
    /// live-queue map is at the global cap. Lets the router shed a new initial
    /// INVITE with a stateless 503 *before* dispatch (ADR-0022 full-guarantee
    /// close). One lock (vs `has_queue` + `queue_count`). Race-safe from the
    /// single-task router: only the router inserts, so between this check and
    /// the dispatch the count can only *fall* (a worker finishing) — never
    /// rise — so a `false` here guarantees the following dispatch is accepted.
    pub fn would_drop_new_at_cap(&self, call_ref: &str) -> bool {
        let map = self.queues.lock().unwrap();
        !map.contains_key(call_ref) && map.len() >= self.cap
    }

    pub fn queue_count(&self) -> usize {
        self.queues.lock().unwrap().len()
    }
}

/// The next item of `call_ref`'s queue: the channel's first, else the
/// overflow's first, else wait on the channel. Channel items are always older
/// than overflow items, so a channel hit needs no lock; an empty channel is
/// re-read under the map lock, where pushes happen, before the overflow.
async fn next_item(
    call_ref: &str,
    rx: &mut mpsc::Receiver<DispatchItem>,
    queues: &QueueMap,
    metrics: &B2buaMetrics,
) -> Option<DispatchItem> {
    if let Ok(item) = rx.try_recv() {
        return Some(item);
    }
    let parked = {
        let mut map = queues.lock().unwrap();
        match rx.try_recv() {
            Ok(item) => Some(item),
            Err(_) => map.get_mut(call_ref).and_then(|q| q.pop_overflow(metrics)),
        }
    };
    match parked {
        Some(item) => Some(item),
        // Empty channel and overflow: the next push lands in the channel.
        None => rx.recv().await,
    }
}

async fn worker(
    call_ref: String,
    mut rx: mpsc::Receiver<DispatchItem>,
    queues: QueueMap,
    semaphore: Arc<Semaphore>,
    metrics: B2buaMetrics,
    failure_hook: Option<FailureHook>,
    inflight: InflightMap,
) {
    // The map holds the only sender until the poison arm removes it, so the
    // worker exits through that arm; `rx` never closes while it loops.
    while let Some(item) = next_item(&call_ref, &mut rx, &queues, &metrics).await {
        match item {
            DispatchItem::Poison(c) => {
                // Nothing is queued behind a release: `push` turns every later
                // item away as `Released`. The entry leaves the map, so a
                // later event for the call_ref starts a fresh queue.
                inflight.lock().unwrap().remove(&call_ref);
                let left = queues.lock().unwrap().remove(&call_ref);
                debug_assert!(
                    left.is_none_or(|q| q.overflow.is_empty()) && rx.try_recv().is_err(),
                    "an item queued behind a release"
                );
                metrics.bump_removal_of(c);
                return;
            }
            DispatchItem::Event(job) => {
                if semaphore.available_permits() == 0 {
                    metrics.bump_saturation();
                }
                let permit = semaphore.clone().acquire_owned().await.expect("semaphore closed");
                // Isolate handler panics/aborts: the worker survives and the
                // failure is REPORTED (ADR-0020 X6 — the pre-reaper swallow
                // here was the "call leaks forever, zero CDR" escape route).
                // The body owns its answer from here: the hook is dropped.
                let task = tokio::spawn(job.body);
                inflight.lock().unwrap().insert(call_ref.clone(), task.abort_handle());
                let outcome = task.await;
                inflight.lock().unwrap().remove(&call_ref);
                match outcome {
                    Err(e) if e.is_panic() => {
                        metrics.bump_handler_panic();
                        if let Some(hook) = &failure_hook {
                            hook(&call_ref, HandlerFailure::Panicked);
                        }
                    }
                    Err(e) if e.is_cancelled() => {
                        if let Some(hook) = &failure_hook {
                            hook(&call_ref, HandlerFailure::Aborted);
                        }
                    }
                    _ => {}
                }
                drop(permit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn preserves_per_call_fifo_order() {
        let d = PerCallDispatcher::new(8, 64, 1024, B2buaMetrics::new());
        let order = Arc::new(Mutex::new(Vec::<u32>::new()));
        let done = Arc::new(Notify::new());
        for i in 0..10u32 {
            let order = order.clone();
            let done = done.clone();
            d.dispatch(
                "w0|cid|tag",
                Job::new(Box::pin(async move {
                    order.lock().unwrap().push(i);
                    if i == 9 {
                        done.notify_one();
                    }
                })),
            )
            .await;
        }
        done.notified().await;
        assert_eq!(*order.lock().unwrap(), (0..10).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn full_queue_drops_and_counts() {
        let metrics = B2buaMetrics::new();
        // depth 1, concurrency 1: a blocked first handler forces drops behind it.
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let gate = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let ran = Arc::new(AtomicU32::new(0));
        {
            let gate = gate.clone();
            let started = started.clone();
            let ran = ran.clone();
            d.dispatch(
                "c",
                Job::new(Box::pin(async move {
                    started.notify_one();
                    gate.notified().await;
                    ran.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .await;
        }
        started.notified().await; // first handler is now parked on the gate
                                  // Queue depth is 1 — one of these sits in the queue, the rest are dropped.
        for _ in 0..5 {
            let ran = ran.clone();
            d.dispatch(
                "c",
                Job::new(Box::pin(async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .await;
        }
        assert!(metrics.queue_drops_total() >= 1, "expected queue drops");
        gate.notify_waiters();
    }

    /// A queue's removal takes the class of the first poison it dequeues: a
    /// terminated call whose queued event then released it again as an orphan
    /// is one `terminated` removal, and the classes sum to the removals.
    #[tokio::test]
    async fn removal_counts_the_first_poison_class() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 8, 1024, metrics.clone());
        let gate = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        {
            let (gate, started) = (gate.clone(), started.clone());
            d.dispatch(
                "a",
                Job::new(Box::pin(async move {
                    started.notify_one();
                    gate.notified().await;
                })),
            )
            .await;
        }
        started.notified().await;
        d.enqueue_poison("a", RemovalClass::Terminated);
        d.enqueue_poison("a", RemovalClass::Orphan);
        d.dispatch("b", Job::new(Box::pin(async {}))).await;
        d.enqueue_poison("b", RemovalClass::Orphan);
        gate.notify_waiters();
        while d.queue_count() > 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(metrics.removals_total(), 2);
        assert_eq!(metrics.removals_of_total(RemovalClass::Terminated), 1);
        assert_eq!(metrics.removals_of_total(RemovalClass::Orphan), 1);
        assert_eq!(metrics.removals_of_total(RemovalClass::SelfRelease), 0);
        let text = metrics.prometheus_text();
        assert!(text.contains("b2bua_call_removals_by_class_total{class=\"terminated\"} 1"));
        assert!(text.contains("b2bua_call_removals_by_class_total{class=\"orphan\"} 1"));
    }

    /// A release reaching a full queue is not lost: the worker still drains
    /// and exits once the bodies ahead of it have run.
    #[tokio::test]
    async fn a_release_reaching_a_full_queue_still_removes_it() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        {
            let (gate, started) = (gate.clone(), started.clone());
            d.dispatch(
                "c",
                Job::new(Box::pin(async move {
                    started.notify_one();
                    gate.notified().await;
                })),
            )
            .await;
        }
        started.notified().await;
        d.dispatch("c", Job::new(Box::pin(async {}))).await;
        d.enqueue_poison("c", RemovalClass::Terminated);
        gate.notify_one();
        for _ in 0..1000 {
            if d.queue_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(d.queue_count(), 0, "the release behind the full queue removes it");
        assert_eq!(metrics.removals_of_total(RemovalClass::Terminated), 1);
    }

    /// A hook that records the reason it hears.
    fn recording_hook(heard: &Arc<Mutex<Vec<Discard>>>) -> Option<DiscardHook> {
        let heard = heard.clone();
        Some(Box::new(move |why| {
            heard.lock().unwrap().push(why);
            Box::pin(async {})
        }))
    }

    /// A body that records `label` in `order`.
    fn record(order: &Arc<Mutex<Vec<&'static str>>>, label: &'static str) -> DispatchBody {
        let order = order.clone();
        Box::pin(async move { order.lock().unwrap().push(label) })
    }

    /// Park a body on call `c` until the returned gate opens.
    async fn park(d: &PerCallDispatcher) -> Arc<Notify> {
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (g, st) = (gate.clone(), started.clone());
        d.dispatch(
            "c",
            Job::new(Box::pin(async move {
                st.notify_one();
                g.notified().await;
            })),
        )
        .await;
        started.notified().await;
        gate
    }

    async fn drained(d: &PerCallDispatcher) {
        for _ in 0..1000 {
            if d.queue_count() == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the queue never drained");
    }

    /// A body that records `label`, then waits for the returned gate.
    fn gated(
        order: &Arc<Mutex<Vec<&'static str>>>,
        label: &'static str,
    ) -> (DispatchBody, Arc<Notify>) {
        let (gate, order) = (Arc::new(Notify::new()), order.clone());
        let g = gate.clone();
        let body: DispatchBody = Box::pin(async move {
            order.lock().unwrap().push(label);
            g.notified().await;
        });
        (body, gate)
    }

    /// A past-bounds job finding the queue full waits past it and runs in
    /// FIFO order; a bounded job offered while the channel is still full is
    /// discarded and its hook hears why.
    #[tokio::test]
    async fn a_past_bounds_job_waits_past_a_full_queue_in_order() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "queued"))).await;
        d.dispatch("c", Job::new(record(&order, "past")).past_bounds()).await;
        assert_eq!(metrics.overflow_depth(), 1);
        d.dispatch("c", Job::new(record(&order, "late")).on_discard(recording_hook(&heard))).await;
        assert_eq!(metrics.past_bound_of_total(PastBound::Depth), 1);
        assert_eq!(metrics.queue_drops_total(), 1);
        assert_eq!(*heard.lock().unwrap(), vec![Discard::QueueFull]);

        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["queued", "past"]);
        assert_eq!(metrics.release_discards_total(), 0);
        assert_eq!(metrics.overflow_depth(), 0);
    }

    /// The worker has emptied the channel and is held on a body while a job
    /// still waits in the overflow. A new bounded job is not refused for it:
    /// the waiting job moves into the channel first, the new one follows,
    /// and both run in their order.
    #[tokio::test]
    async fn an_overflowed_job_moves_into_the_freed_channel_ahead_of_a_new_one() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 2, 1024, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        let (held, release_held) = gated(&order, "held");
        d.dispatch("c", Job::new(record(&order, "first"))).await;
        d.dispatch("c", Job::new(held)).await;
        d.dispatch("c", Job::new(record(&order, "waiting")).past_bounds()).await;
        assert_eq!(metrics.overflow_depth(), 1);

        gate.notify_one();
        while order.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        // The channel is empty, the worker is inside `held`, `waiting` is in
        // the overflow.
        let heard = Arc::new(Mutex::new(Vec::new()));
        d.dispatch("c", Job::new(record(&order, "new")).on_discard(recording_hook(&heard))).await;
        assert!(heard.lock().unwrap().is_empty(), "a freed channel slot takes the new job");
        assert_eq!(metrics.overflow_depth(), 0);

        release_held.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["first", "held", "waiting", "new"]);
    }

    /// A release parked in the overflow: a job offered after it is turned
    /// away as behind the release, not as for a full queue.
    #[tokio::test]
    async fn a_job_behind_a_parked_release_hears_the_release() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "queued"))).await;
        d.enqueue_poison("c", RemovalClass::Terminated);
        assert_eq!(metrics.overflow_depth(), 1, "the release waits past the full channel");
        d.dispatch("c", Job::new(record(&order, "behind")).on_discard(recording_hook(&heard)))
            .await;
        assert_eq!(*heard.lock().unwrap(), vec![Discard::Released(RemovalClass::Terminated)]);
        assert_eq!(metrics.queue_drops_total(), 0);
        assert_eq!(metrics.release_discards_total(), 1);

        gate.notify_one();
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["queued"]);
    }

    /// A stalled call flooded with past-bounds jobs: its overflow holds at
    /// most its queue depth of them, every one past that is discarded as
    /// for a full queue and the overflow hook hears the call each time. A
    /// job that waits past every bound is still queued.
    #[tokio::test]
    async fn a_flood_of_past_bounds_jobs_stops_at_the_overflow_ceiling() {
        let metrics = B2buaMetrics::new();
        let condemned = Arc::new(Mutex::new(Vec::new()));
        let c = condemned.clone();
        let d = PerCallDispatcher::new(1, 2, 1024, metrics.clone()).with_overflow_hook(Arc::new(
            move |call_ref: &str| c.lock().unwrap().push(call_ref.to_string()),
        ));
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        for _ in 0..1000 {
            d.dispatch(
                "c",
                Job::new(record(&order, "flood")).past_bounds().on_discard(recording_hook(&heard)),
            )
            .await;
            assert!(metrics.overflow_depth() <= 2, "the overflow stays at its ceiling");
        }
        // Two in the channel, two in the overflow; the rest turned away.
        assert_eq!(metrics.overflow_depth(), 2);
        assert_eq!(metrics.overflow_refused_total(), 996);
        assert_eq!(metrics.queue_drops_total(), 996);
        assert_eq!(heard.lock().unwrap().len(), 996);
        assert!(heard.lock().unwrap().iter().all(|w| *w == Discard::QueueFull));
        assert_eq!(condemned.lock().unwrap().len(), 996);
        assert!(condemned.lock().unwrap().iter().all(|r| r == "c"));

        d.dispatch("c", Job::new(record(&order, "verdict")).past_all_bounds()).await;
        assert_eq!(metrics.overflow_depth(), 3, "past every bound, the ceiling included");

        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(order.lock().unwrap().len(), 5);
        assert_eq!(order.lock().unwrap().last(), Some(&"verdict"));
        assert_eq!(metrics.overflow_depth(), 0);
    }

    /// At the global cap a bounded job for a new call is discarded (its hook
    /// hears it); a past-bounds one opens its queue anyway, and is counted.
    #[tokio::test]
    async fn a_past_bounds_job_opens_a_queue_past_the_cap() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 8, 1, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("n", Job::new(record(&order, "bounded")).on_discard(recording_hook(&heard)))
            .await;
        assert_eq!(*heard.lock().unwrap(), vec![Discard::AtCap]);
        assert_eq!(metrics.cap_drops_total(), 1);

        d.dispatch("n", Job::new(record(&order, "past")).past_bounds()).await;
        assert_eq!(metrics.past_bound_of_total(PastBound::Cap), 1);
        d.enqueue_poison("n", RemovalClass::Orphan);
        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["past"]);
    }

    /// Every job offered after a release is discarded, a past-bounds one
    /// included, and its hook hears the release's class; a job that ran never
    /// hears its hook.
    #[tokio::test]
    async fn jobs_behind_a_release_hear_it() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let heard = Arc::new(Mutex::new(Vec::new()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "ran")).on_discard(recording_hook(&heard))).await;
        d.enqueue_poison("c", RemovalClass::Terminated);
        d.dispatch(
            "c",
            Job::new(record(&order, "behind")).past_bounds().on_discard(recording_hook(&heard)),
        )
        .await;
        gate.notify_one();
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["ran"]);
        assert_eq!(*heard.lock().unwrap(), vec![Discard::Released(RemovalClass::Terminated)]);
        assert_eq!(metrics.release_discards_total(), 1);
        assert_eq!(metrics.removals_of_total(RemovalClass::Terminated), 1);
    }

    /// Jobs queued past every bound do not use up the call's allowance of
    /// past-bounds jobs: a `past_bounds` job still finds room behind them.
    #[tokio::test]
    async fn jobs_past_all_bounds_leave_the_ceiling_to_past_bounds_jobs() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "queued"))).await;
        d.dispatch("c", Job::new(record(&order, "v1")).past_all_bounds()).await;
        d.dispatch("c", Job::new(record(&order, "v2")).past_all_bounds()).await;
        d.dispatch(
            "c",
            Job::new(record(&order, "cancelled")).past_bounds().on_discard(recording_hook(&heard)),
        )
        .await;
        assert!(heard.lock().unwrap().is_empty(), "the past-bounds job finds its allowance");
        assert_eq!(metrics.overflow_refused_total(), 0);

        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["queued", "v1", "v2", "cancelled"]);
        assert_eq!(metrics.overflow_depth(), 0);
    }

    /// A dispatcher whose calls may be offered `cap` counted jobs, recording
    /// the calls that cross it.
    fn capped(cap: u64, metrics: &B2buaMetrics) -> (PerCallDispatcher, Arc<Mutex<Vec<String>>>) {
        let crossed = Arc::new(Mutex::new(Vec::new()));
        let c = crossed.clone();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone()).with_lifetime_cap(
            cap,
            Arc::new(move |call_ref: &str| c.lock().unwrap().push(call_ref.to_string())),
        );
        (d, crossed)
    }

    /// A stuck call still counts every job it is offered — queued or
    /// discarded at the full queue. The one that crosses the cap is turned
    /// away as behind a release, the hook hears the call once, later jobs are
    /// turned away too, and a job past every bound still gets in.
    #[tokio::test]
    async fn a_stuck_calls_queued_and_discarded_jobs_count_toward_its_lifetime_cap() {
        let metrics = B2buaMetrics::new();
        let (d, crossed) = capped(3, &metrics);
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "queued"))).await;
        d.dispatch("c", Job::new(record(&order, "full")).on_discard(recording_hook(&heard))).await;
        assert!(crossed.lock().unwrap().is_empty(), "three jobs offered, none past the cap");
        d.dispatch("c", Job::new(record(&order, "fourth")).on_discard(recording_hook(&heard)))
            .await;
        d.dispatch("c", Job::new(record(&order, "fifth")).on_discard(recording_hook(&heard))).await;
        assert_eq!(*crossed.lock().unwrap(), vec!["c".to_string()], "heard once");
        assert_eq!(metrics.message_cap_lifetime_terminated_total(), 1);
        let released = Discard::Released(RemovalClass::Terminated);
        assert_eq!(*heard.lock().unwrap(), vec![Discard::QueueFull, released, released]);

        d.dispatch("c", Job::new(record(&order, "verdict")).own().past_all_bounds()).await;
        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["queued", "verdict"]);
    }

    /// The node's own jobs — its timers, its verdicts — never count toward
    /// the cap, however many there are.
    #[tokio::test]
    async fn the_nodes_own_jobs_do_not_count_toward_the_lifetime_cap() {
        let metrics = B2buaMetrics::new();
        let (d, crossed) = capped(1, &metrics);
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        for _ in 0..10 {
            d.dispatch("c", Job::new(record(&order, "timer")).own()).await;
            d.dispatch("c", Job::new(record(&order, "verdict")).own().past_all_bounds()).await;
        }
        assert!(crossed.lock().unwrap().is_empty());
        assert_eq!(metrics.message_cap_lifetime_terminated_total(), 0);
        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
    }
}
