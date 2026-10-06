//! The per-call dispatcher (ADR-0010 X2): each call's events run strictly in
//! order on the call's own worker, and a global permit pool caps the handler
//! bodies in flight, so a slow handler on one call never blocks another. New
//! normal calls hold at most their share of that pool.
//!
//! - [`class`] — the dispatch table: the [`DispatchClass`] of an event and
//!   its [`Row`]: room, lifetime-cap counting, what a discard owes.
//! - [`queue`] — [`CallQueue`], one call's synchronous queue: FIFO, depth,
//!   overflow ceiling, lifetime cap.
//! - [`worker`] — the tokio adapter: one worker task per call, permits,
//!   panic isolation, abort.
//!
//! [`PerCallDispatcher::offer`] is the one way in. It never awaits: an item
//! is queued, or handed back [`Discarded`] with why and what is
//! [owed](Owed), for the caller to render. The offer also states the facts
//! the caller acts on — the call crossed its lifetime cap, or its overflow
//! reached its ceiling.
//!
//! Three bounds hold per call: the queue depth with its overflow ceiling, the
//! lifetime cap here, and the per-interval message cap on the call's turn
//! (`router::process`). The global cap bounds the live queues: an event of a
//! bounded row for a call with no queue is refused at its row's
//! [threshold](QueueThreshold) — the cap, or for a normal new INVITE the cap
//! less the new-call headroom; a past-bounds or always-room event opens the
//! call's queue past it. The threshold check and the queue's creation happen
//! under one map lock in one offer.

pub mod class;
pub mod queue;
pub mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use class::{DispatchClass, Owed, PermitPool, QueueThreshold, Room, Row};
pub use queue::{CallQueue, Discard, Discarded, Limits, Offer, Outcome, PastBound};
pub use worker::{DispatchBody, FailureHook, HandlerFailure, InFlight, Runnable};

use crate::metrics::{B2buaMetrics, RemovalClass};
use worker::{Permits, Slot, Slots, WorkerCtx};

/// The dispatcher handle over items of type `T`. Clone-cheap.
pub struct PerCallDispatcher<T> {
    slots: Slots<T>,
    permits: Permits,
    limits: Limits,
    cap: usize,
    new_call_headroom: usize,
    metrics: B2buaMetrics,
    failure_hook: Option<FailureHook>,
    in_flight: InFlight,
}

impl<T> Clone for PerCallDispatcher<T> {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            permits: self.permits.clone(),
            limits: self.limits,
            cap: self.cap,
            new_call_headroom: self.new_call_headroom,
            metrics: self.metrics.clone(),
            failure_hook: self.failure_hook.clone(),
            in_flight: self.in_flight.clone(),
        }
    }
}

impl<T: Runnable> PerCallDispatcher<T> {
    /// `concurrency` handler bodies in flight at most, `depth` items per
    /// call queue (and as many past-bounds items in its overflow), `cap` live
    /// call queues. No lifetime cap until [`with_lifetime_cap`](Self::with_lifetime_cap),
    /// and new calls bounded by `concurrency` and `cap` alone until
    /// [`with_new_call_bounds`](Self::with_new_call_bounds).
    pub fn new(concurrency: usize, depth: usize, cap: usize, metrics: B2buaMetrics) -> Self {
        Self {
            slots: Arc::new(Mutex::new(HashMap::new())),
            permits: Permits::new(concurrency),
            limits: Limits { depth: depth.max(1), lifetime_cap: u64::MAX },
            cap: cap.max(1),
            new_call_headroom: 0,
            metrics,
            failure_hook: None,
            in_flight: InFlight::default(),
        }
    }

    /// Install the handler-failure hook (builder-style; the call reaper's
    /// two-strike escalation — ADR-0020 X6).
    pub fn with_failure_hook(mut self, hook: FailureHook) -> Self {
        self.failure_hook = Some(hook);
        self
    }

    /// Bound new normal calls (builder-style): their bodies hold at most
    /// `permits` of the handler permits at once (at least one), and their
    /// INVITE opens a queue only below the global cap less `headroom` (kept
    /// below the cap, so one queue stays open to them).
    pub fn with_new_call_bounds(mut self, permits: usize, headroom: usize) -> Self {
        self.permits = self.permits.with_new_call_share(permits);
        self.new_call_headroom = headroom.min(self.cap - 1);
        self
    }

    /// Bound the counted offers a call may receive over its life (builder-
    /// style); an offer behind the call's queued release is not counted. The
    /// offer that crosses `cap`, and every later one its row does
    /// not [admit when capped](Row::admitted_when_capped), is discarded as
    /// [`Discard::Capped`]; the crossing offer says so, once, and its caller
    /// must end the call: nothing else ends a capped call.
    pub fn with_lifetime_cap(mut self, cap: u64) -> Self {
        self.limits.lifetime_cap = cap;
        self
    }

    /// Offer `item` of `class` to `call_ref`'s queue, opening the queue and
    /// its worker when the call has none.
    pub fn offer(&self, call_ref: &str, item: T, class: DispatchClass) -> Offer<T> {
        let row = class.row();
        let mut slots = self.slots.lock().unwrap();
        let (slot, offer) = match slots.get(call_ref).cloned() {
            Some(slot) => {
                // Offered under the map lock: the worker taking the queue's
                // release decides under it whether the queue lives on.
                let offer = slot.queue.lock().unwrap().offer(item, class, &self.metrics);
                drop(slots);
                (slot, offer)
            }
            None => {
                if slots.len() >= row.opens_queue_below.of(self.cap, self.new_call_headroom) {
                    if row.room == Room::Bounded {
                        // A new call's discard is counted once, as a refused
                        // new call, by the router that answers it.
                        if !class.is_new_call() {
                            self.metrics.bump_cap_drop();
                        }
                        return Offer::discarded(item, Discard::AtCap, class);
                    }
                    self.metrics.bump_past_bound(PastBound::Cap);
                }
                let mut queue = CallQueue::new(self.limits);
                let offer = queue.offer(item, class, &self.metrics);
                let slot = Arc::new(Slot::new(queue));
                slots.insert(call_ref.to_string(), slot.clone());
                drop(slots);
                self.metrics.bump_creation();
                tokio::spawn(worker::run(call_ref.to_string(), slot.clone(), self.worker_ctx()));
                (slot, offer)
            }
        };
        if let Outcome::Queued = offer.outcome {
            slot.wake();
        }
        if offer.crossed_lifetime_cap {
            self.metrics.bump_message_cap_lifetime_crossed();
            tracing::warn!(
                call_ref,
                cap = self.limits.lifetime_cap,
                "call crossed its lifetime message cap: ending it"
            );
        }
        offer
    }

    /// Whether an event of `class` would find no room at the global cap now:
    /// `call_ref` has no queue, the live queues reach the class's threshold,
    /// and its room is bounded. The admission ladder's shed rung reads it
    /// ahead of the offer, which stays the one place a queue opens.
    pub fn at_threshold(&self, call_ref: &str, class: DispatchClass) -> bool {
        let row = class.row();
        let slots = self.slots.lock().unwrap();
        row.room == Room::Bounded
            && !slots.contains_key(call_ref)
            && slots.len() >= row.opens_queue_below.of(self.cap, self.new_call_headroom)
    }

    /// Queue `call_ref`'s release behind every item already queued, a full
    /// queue included, or in front of the first new call queued: its worker
    /// drains what is ahead of it, then exits and removes the queue, or runs
    /// what waits behind it as the call's next queue. The first release names
    /// the removal's class; a second release is ignored, and a later offer is
    /// discarded behind it unless a new call waits there. No-op for a call
    /// with no queue.
    pub fn release(&self, call_ref: &str, class: RemovalClass) {
        let slot = self.slots.lock().unwrap().get(call_ref).cloned();
        if let Some(slot) = slot {
            slot.queue.lock().unwrap().release(class, &self.metrics);
            slot.wake();
        }
    }

    fn worker_ctx(&self) -> WorkerCtx<T> {
        WorkerCtx {
            slots: self.slots.clone(),
            permits: self.permits.clone(),
            metrics: self.metrics.clone(),
            failure_hook: self.failure_hook.clone(),
            in_flight: self.in_flight.clone(),
        }
    }
}

impl<T> PerCallDispatcher<T> {
    /// Abort `call_ref`'s in-flight handler body; see [`InFlight::abort`].
    pub fn abort_in_flight(&self, call_ref: &str) -> bool {
        self.in_flight.abort(call_ref)
    }

    /// The handle on every call's in-flight body.
    pub fn in_flight(&self) -> InFlight {
        self.in_flight.clone()
    }

    pub fn has_queue(&self, call_ref: &str) -> bool {
        self.slots.lock().unwrap().contains_key(call_ref)
    }

    pub fn queue_count(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

#[cfg(test)]
mod expected;
#[cfg(test)]
mod properties;
#[cfg(test)]
mod worker_tests;
