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

/// A body offered to [`PerCallDispatcher::dispatch`], with what becomes of it
/// if it never runs.
pub struct Job {
    body: DispatchBody,
    on_discard: Option<DiscardHook>,
    past_bounds: bool,
}

impl Job {
    /// A body discarded silently when there is no room for it.
    pub fn new(body: DispatchBody) -> Self {
        Self { body, on_discard: None, past_bounds: false }
    }

    /// `hook` runs in place of the body if it is discarded unrun; dropped
    /// unheard once the body starts.
    pub fn on_discard(mut self, hook: Option<DiscardHook>) -> Self {
        self.on_discard = hook;
        self
    }

    /// Queued past a full per-call queue and past the global cap, in FIFO
    /// order; discarded only behind the call's release. The caller bounds how
    /// many such jobs it offers.
    pub fn past_bounds(mut self) -> Self {
        self.past_bounds = true;
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
    /// Never discarded for want of room: a release, or a past-bounds job.
    fn is_past_bounds(&self) -> bool {
        match self {
            DispatchItem::Event(job) => job.past_bounds,
            DispatchItem::Poison(_) => true,
        }
    }
}

/// A call's queue: the bounded channel its worker reads, then the items
/// admitted past it. Every item in `overflow` is younger than every item in
/// the channel: while `overflow` holds anything, new items join it (or are
/// discarded), and the worker takes from it only once the channel is empty.
struct PerCallQueue {
    tx: mpsc::Sender<DispatchItem>,
    overflow: VecDeque<DispatchItem>,
}

impl PerCallQueue {
    /// Queue `item` in FIFO order; a bounded item finding no room comes back.
    fn push(&mut self, item: DispatchItem, metrics: &B2buaMetrics) -> Result<(), DispatchItem> {
        let item = if self.overflow.is_empty() {
            match self.tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(item))
                | Err(mpsc::error::TrySendError::Closed(item)) => item,
            }
        } else {
            item
        };
        if !item.is_past_bounds() {
            return Err(item);
        }
        metrics.bump_past_bound(PastBound::Depth);
        self.overflow.push_back(item);
        Ok(())
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
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Install the handler-failure hook (builder-style; the call reaper's
    /// two-strike escalation — ADR-0020 X6).
    pub fn with_failure_hook(mut self, hook: FailureHook) -> Self {
        self.failure_hook = Some(hook);
        self
    }

    /// Abort the currently in-flight handler body for `call_ref` (ADR-0020 X6
    /// escalation rung): a hung body holds both the worker and the per-call
    /// lock; aborting drops the body future — releasing the lock guard — and
    /// the worker observes the cancellation and reports
    /// [`HandlerFailure::Aborted`]. No-op when the call has no body in flight.
    pub fn abort_in_flight(&self, call_ref: &str) {
        if let Some(h) = self.inflight.lock().unwrap().get(call_ref) {
            h.abort();
        }
    }

    /// Enqueue `job` for `call_ref`, lazily creating the queue + worker. A
    /// bounded job finding the per-call queue full or the global cap reached
    /// is discarded unrun and counted, and its hook is awaited here.
    pub async fn dispatch(&self, call_ref: &str, job: Job) {
        if let Some((why, job)) = self.enqueue(call_ref, job) {
            job.discard(why).await;
        }
    }

    /// The synchronous half of [`dispatch`](Self::dispatch), under the map
    /// lock: the job queued, or handed back with why it was not.
    fn enqueue(&self, call_ref: &str, job: Job) -> Option<(Discard, Job)> {
        let mut map = self.queues.lock().unwrap();
        if let Some(q) = map.get_mut(call_ref) {
            return match q.push(DispatchItem::Event(job), &self.metrics) {
                Ok(()) => None,
                Err(DispatchItem::Event(job)) => {
                    self.metrics.bump_queue_drop();
                    Some((Discard::QueueFull, job))
                }
                Err(DispatchItem::Poison(_)) => unreachable!("an event was pushed"),
            };
        }
        if map.len() >= self.cap {
            if !job.past_bounds {
                self.metrics.bump_cap_drop();
                return Some((Discard::AtCap, job));
            }
            self.metrics.bump_past_bound(PastBound::Cap);
        }
        let (tx, rx) = mpsc::channel(self.depth);
        // Send before spawning the worker: capacity is fresh so this can't fail.
        let _ = tx.try_send(DispatchItem::Event(job));
        map.insert(call_ref.to_string(), PerCallQueue { tx, overflow: VecDeque::new() });
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
        None
    }

    /// Signal the worker for `call_ref` to drain and exit (call eviction),
    /// after every item already queued — a full queue included. The first
    /// poison the worker dequeues names the removal's class; a later one (a
    /// release by an event queued ahead of it) is discarded with the queue.
    pub fn enqueue_poison(&self, call_ref: &str, class: RemovalClass) {
        let mut map = self.queues.lock().unwrap();
        if let Some(q) = map.get_mut(call_ref) {
            let _ = q.push(DispatchItem::Poison(class), &self.metrics);
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
) -> Option<DispatchItem> {
    if let Ok(item) = rx.try_recv() {
        return Some(item);
    }
    let parked = {
        let mut map = queues.lock().unwrap();
        match rx.try_recv() {
            Ok(item) => Some(item),
            Err(_) => map.get_mut(call_ref).and_then(|q| q.overflow.pop_front()),
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
    while let Some(item) = next_item(&call_ref, &mut rx, &queues).await {
        match item {
            DispatchItem::Poison(c) => {
                // The entry leaves the map first, so no `dispatch` can land in
                // this queue after the drain: every body queued behind the
                // release is counted and discarded unrun, its hook heard, and
                // a later event for the call_ref starts a fresh queue.
                inflight.lock().unwrap().remove(&call_ref);
                let overflow = queues
                    .lock()
                    .unwrap()
                    .remove(&call_ref)
                    .map(|q| q.overflow)
                    .unwrap_or_default();
                let mut behind = Vec::new();
                while let Ok(item) = rx.try_recv() {
                    behind.push(item);
                }
                behind.extend(overflow);
                for item in behind {
                    if let DispatchItem::Event(job) = item {
                        metrics.bump_release_discard();
                        job.discard(Discard::Released(c)).await;
                    }
                }
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

    /// A past-bounds job finding the queue full waits past it and runs in
    /// FIFO order; a bounded job offered behind it is discarded and its hook
    /// hears why, even once the channel has room again.
    #[tokio::test]
    async fn a_past_bounds_job_waits_past_a_full_queue_in_order() {
        let metrics = B2buaMetrics::new();
        let d = PerCallDispatcher::new(1, 1, 1024, metrics.clone());
        let order = Arc::new(Mutex::new(Vec::new()));
        let heard = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "queued"))).await;
        d.dispatch("c", Job::new(record(&order, "past")).past_bounds()).await;
        d.dispatch("c", Job::new(record(&order, "late")).on_discard(recording_hook(&heard))).await;
        assert_eq!(metrics.past_bound_of_total(PastBound::Depth), 1);
        assert_eq!(metrics.queue_drops_total(), 1);
        assert_eq!(*heard.lock().unwrap(), vec![Discard::QueueFull]);

        gate.notify_one();
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        assert_eq!(*order.lock().unwrap(), vec!["queued", "past"]);
        assert_eq!(metrics.release_discards_total(), 0);
    }

    /// While a job waits past the full queue, the channel frees as the worker
    /// runs; a later past-bounds job still queues behind the first, never
    /// ahead of it.
    #[tokio::test]
    async fn past_bounds_jobs_keep_their_order_as_the_channel_frees() {
        let d = PerCallDispatcher::new(1, 1, 1024, B2buaMetrics::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = park(&d).await;
        d.dispatch("c", Job::new(record(&order, "a"))).await;
        d.dispatch("c", Job::new(record(&order, "b")).past_bounds()).await;
        gate.notify_one();
        tokio::task::yield_now().await;
        d.dispatch("c", Job::new(record(&order, "c")).past_bounds()).await;
        d.dispatch("c", Job::new(record(&order, "d"))).await;
        d.enqueue_poison("c", RemovalClass::Terminated);
        drained(&d).await;
        let order = order.lock().unwrap().clone();
        assert_eq!(&order[..3], &["a", "b", "c"], "FIFO holds across the overflow");
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

    /// Behind a release every job is discarded, a past-bounds one included,
    /// and each hook hears the release's class; a job that ran never hears
    /// its hook.
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
}
