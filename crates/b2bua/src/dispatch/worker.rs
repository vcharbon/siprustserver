//! The per-call worker: the tokio adapter over a [`CallQueue`]. One task per
//! call takes the queue's items in order and runs each as a handler body on a
//! spawned task it awaits, holding the permits of the item's
//! [pool](super::class::PermitPool) for the body's whole run. A panicking
//! body is isolated (`JoinError`) and reported; a hung one can be aborted
//! from outside ([`InFlight::abort`]). The worker exits on the call's
//! release, taking the queue out of the dispatcher's map, unless a new call
//! waits behind the release: it then runs the call's next queue.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::class::PermitPool;
use super::queue::{CallQueue, Popped};
use crate::metrics::B2buaMetrics;
use crate::poison::{locked, DISPATCH_BODIES, DISPATCH_QUEUE, DISPATCH_SLOTS};

/// A handler body: a self-contained future the worker runs on its own task.
pub type DispatchBody = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// An item a call's worker can run.
pub trait Runnable: Send + 'static {
    /// The body that runs this item. Called by the worker once it holds the
    /// item's permits; the body may be dropped unpolled if it is aborted
    /// before its first poll.
    fn into_body(self) -> DispatchBody;
}

impl Runnable for DispatchBody {
    fn into_body(self) -> DispatchBody {
        self
    }
}

/// Why a handler body died (ADR-0020 X6). `Panicked` = the spawned body's
/// `JoinError::is_panic()`; `Aborted` = the call reaper's escalation cancelled
/// a hung body via [`InFlight::abort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerFailure {
    Panicked,
    Aborted,
}

/// Notified from the per-call worker when a handler body failed — after the
/// body unwound (its per-call lock guard is released) and before the worker
/// takes the next item, so notifications are FIFO-ordered with the call's
/// own events. Must be cheap and non-blocking (it runs on the worker). The
/// call reaper installs the only production hook (two-strike escalation).
pub type FailureHook = Arc<dyn Fn(&str, HandlerFailure) + Send + Sync>;

/// The handler body each call has in flight (FIFO: at most one), by call,
/// the tasks doing call work beside them, and whether the dispatcher halted.
/// Clone-cheap.
#[derive(Clone, Default)]
pub struct InFlight {
    bodies: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    beside: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    halted: Arc<AtomicBool>,
}

impl InFlight {
    /// Abort `call_ref`'s in-flight body (ADR-0020 X6 escalation rung): a
    /// hung body holds both the worker and the per-call lock; aborting drops
    /// the body future — releasing the lock guard — and the worker reports
    /// [`HandlerFailure::Aborted`]. `true` when there was a body to abort.
    pub fn abort(&self, call_ref: &str) -> bool {
        match locked(&self.bodies, DISPATCH_BODIES).get(call_ref) {
            Some(h) => {
                h.abort();
                true
            }
            None => false,
        }
    }

    /// Abort every body in flight and every task beside them, and run no item
    /// after: the node crashed.
    pub fn halt(&self) {
        self.halted.store(true, Ordering::SeqCst);
        for handle in locked(&self.bodies, DISPATCH_BODIES).values() {
            handle.abort();
        }
        for handle in locked(&self.beside, DISPATCH_BODIES).iter() {
            handle.abort();
        }
    }

    /// Track `handle`, a task doing call work outside any call's queue, so a
    /// halt aborts it too; aborted at once after a halt.
    pub fn track(&self, handle: tokio::task::AbortHandle) {
        let mut beside = locked(&self.beside, DISPATCH_BODIES);
        beside.retain(|h| !h.is_finished());
        if self.halted() {
            handle.abort();
        }
        beside.push(handle);
    }

    fn halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    fn insert(&self, call_ref: &str, handle: tokio::task::AbortHandle) {
        locked(&self.bodies, DISPATCH_BODIES).insert(call_ref.to_string(), handle);
    }

    fn remove(&self, call_ref: &str) {
        locked(&self.bodies, DISPATCH_BODIES).remove(call_ref);
    }
}

/// The permit pools handler bodies draw from. A new-call body takes its
/// new-call permit before its shared one, and no other body waits on the
/// new-call pool, so no two bodies wait on each other's permits.
#[derive(Clone)]
pub(super) struct Permits {
    concurrency: usize,
    shared: Arc<Semaphore>,
    new_call: Arc<Semaphore>,
}

/// The permits one handler body holds for its whole run.
pub(super) struct Held {
    _new_call: Option<OwnedSemaphorePermit>,
    _shared: OwnedSemaphorePermit,
}

impl Permits {
    /// `concurrency` shared permits, all of them open to new calls.
    pub(super) fn new(concurrency: usize) -> Self {
        let concurrency = concurrency.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            concurrency,
            shared: Arc::new(semaphore(concurrency)),
            new_call: Arc::new(semaphore(concurrency)),
        }
    }

    /// These pools with a new-call share of `permits`, at least one and at
    /// most the shared pool.
    pub(super) fn with_new_call_share(self, permits: usize) -> Self {
        let permits = permits.clamp(1, self.concurrency);
        Self { new_call: Arc::new(semaphore(permits)), ..self }
    }

    /// Wait for the permits of `pool`. A wait on the exhausted new-call share
    /// and one on the exhausted shared pool are each counted once.
    async fn acquire(&self, pool: PermitPool, metrics: &B2buaMetrics) -> Held {
        let new_call = match pool {
            PermitPool::Shared => None,
            PermitPool::NewCall => {
                Some(take(&self.new_call, || metrics.bump_new_call_share_wait()).await)
            }
        };
        Held {
            _new_call: new_call,
            _shared: take(&self.shared, || metrics.bump_saturation()).await,
        }
    }
}

/// A pool of `permits`, at most the semaphore's ceiling.
fn semaphore(permits: usize) -> Semaphore {
    #[expect(clippy::disallowed_methods, reason = "the count is held within the ceiling")]
    let pool = Semaphore::new(permits.min(Semaphore::MAX_PERMITS));
    pool
}

/// A permit of `pool`, calling `waits` first when none is free.
async fn take(pool: &Arc<Semaphore>, waits: impl FnOnce()) -> OwnedSemaphorePermit {
    if pool.available_permits() == 0 {
        waits();
    }
    #[expect(clippy::expect_used, reason = "the pools of `Permits` are never closed")]
    let permit = pool.clone().acquire_owned().await.expect("semaphore closed");
    permit
}

/// One call's queue and the wake-up of its worker. Pushes and pops take the
/// queue's own lock; the dispatcher's map lock only creates or removes a
/// slot.
pub(super) struct Slot<T> {
    pub(super) queue: Mutex<CallQueue<T>>,
    wake: Notify,
}

impl<T> Slot<T> {
    pub(super) fn new(queue: CallQueue<T>) -> Self {
        Self { queue: Mutex::new(queue), wake: Notify::new() }
    }

    /// Wake the worker for an entry just queued. A wake with no worker
    /// waiting is kept for its next wait, so none is lost.
    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }
}

pub(super) type Slots<T> = Arc<Mutex<HashMap<String, Arc<Slot<T>>>>>;

/// What a worker shares with its dispatcher.
pub(super) struct WorkerCtx<T> {
    pub(super) slots: Slots<T>,
    pub(super) permits: Permits,
    pub(super) metrics: B2buaMetrics,
    pub(super) failure_hook: Option<FailureHook>,
    pub(super) in_flight: InFlight,
}

/// Run `slot`'s items for `call_ref` in order until a release with nothing
/// behind it.
pub(super) async fn run<T: Runnable>(call_ref: String, slot: Arc<Slot<T>>, ctx: WorkerCtx<T>) {
    loop {
        let next = locked(&slot.queue, DISPATCH_QUEUE).pop(&ctx.metrics);
        match next {
            None => slot.wake.notified().await,
            Some(Popped::Release(class)) => {
                // What waits behind the release opens the call's next queue;
                // with nothing there the slot leaves the map, so a later event
                // for the call_ref opens a fresh queue. Decided under the map
                // lock, which every offer holds: none lands on a queue whose
                // worker left.
                ctx.in_flight.remove(&call_ref);
                let next = {
                    let mut slots = locked(&ctx.slots, DISPATCH_SLOTS);
                    let next = locked(&slot.queue, DISPATCH_QUEUE).reopen(&ctx.metrics);
                    if !next {
                        slots.remove(&call_ref);
                    }
                    next
                };
                ctx.metrics.bump_removal_of(class);
                if !next {
                    return;
                }
                ctx.metrics.bump_creation();
            }
            Some(Popped::Item(_, _)) if ctx.in_flight.halted() => return,
            Some(Popped::Item(item, class)) => {
                let held = ctx.permits.acquire(class.row().pool, &ctx.metrics).await;
                let task = observe::spawn_keeping_node(item.into_body());
                ctx.in_flight.insert(&call_ref, task.abort_handle());
                let outcome = task.await;
                ctx.in_flight.remove(&call_ref);
                // The worker survives a failed body and reports it (ADR-0020
                // X6): a swallowed failure would leak the call.
                let failure = match outcome {
                    Err(e) if e.is_panic() => {
                        ctx.metrics.bump_handler_panic();
                        // The panic hook names the site; this names the call.
                        tracing::error!(call_ref = %call_ref, "a handler body panicked; the reaper takes the call");
                        Some(HandlerFailure::Panicked)
                    }
                    Err(e) if e.is_cancelled() => Some(HandlerFailure::Aborted),
                    _ => None,
                };
                if let (Some(failure), Some(hook)) = (failure, &ctx.failure_hook) {
                    hook(&call_ref, failure);
                }
                drop(held);
            }
        }
    }
}
