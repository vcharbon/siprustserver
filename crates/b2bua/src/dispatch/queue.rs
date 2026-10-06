//! [`CallQueue`] — one call's queue, synchronous and runtime-free: the
//! events waiting for the call's worker, in FIFO order, and the call's
//! lifetime count. An [offer](CallQueue::offer) applies the event's
//! [`Row`](super::class::Row) and either queues the item or hands it back
//! with why and what is owed.
//!
//! One deque holds every waiting item, whatever its room, so FIFO order
//! across rooms holds by construction. The first `depth` items are the
//! queue proper; any item past them is in the call's **overflow**. A
//! past-bounds item enters the overflow while fewer than `ceiling` (the
//! depth) past-bounds items are in it; an always-room item and the release
//! enter it unconditionally.
//!
//! A new call never runs on the queue its `callRef`'s previous call
//! releases: the release goes in front of the first new call queued, and a
//! new call offered behind the release waits there too. Every offer after
//! that new call waits with it, and the worker that takes the release runs
//! them on the call's [next queue](CallQueue::reopen). The release cutting in
//! can move one past-bounds item into an overflow at its ceiling.

use std::collections::VecDeque;

use super::class::{DispatchClass, Owed, Room};
use crate::metrics::{B2buaMetrics, RemovalClass};

/// The share of the lifetime cap, in percent, past which a call is counted
/// as near it.
const NEAR_CAP_PERCENT: u64 = 80;

/// Why an item was discarded without running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// The call's queue was full.
    QueueFull,
    /// The call had no queue and the global cap was reached.
    AtCap,
    /// It was offered behind the call's release, of this class, with no new
    /// call waiting there.
    Released(RemovalClass),
    /// The call crossed its lifetime cap: it runs no more such work, and is
    /// being ended.
    Capped,
}

/// Which bound an item was queued past (metric label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PastBound {
    /// The call's queue was full.
    Depth,
    /// The global queue cap was reached.
    Cap,
}

/// A call queue's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Items the queue proper holds; also the overflow ceiling.
    pub depth: usize,
    /// Counted offers a call may receive over its life.
    pub lifetime_cap: u64,
}

/// An item handed back unrun.
#[derive(Debug)]
pub struct Discarded<T> {
    pub item: T,
    pub why: Discard,
    pub owed: Owed,
}

/// What became of an offered item.
#[derive(Debug)]
pub enum Outcome<T> {
    /// Queued: the call's worker runs it in FIFO order unless the worker's
    /// body is aborted.
    Queued,
    Discarded(Discarded<T>),
}

/// The result of one offer: the item's outcome, and the facts about the call
/// the offer established.
#[derive(Debug)]
pub struct Offer<T> {
    pub outcome: Outcome<T>,
    /// This offer took the call past its lifetime cap; true for one offer
    /// per queue at most.
    pub crossed_lifetime_cap: bool,
    /// A past-bounds item found the call's overflow at its ceiling: more
    /// such events arrive than the call consumes.
    pub hit_overflow_ceiling: bool,
}

impl<T> Offer<T> {
    pub(super) fn queued(crossed_lifetime_cap: bool) -> Self {
        Offer { outcome: Outcome::Queued, crossed_lifetime_cap, hit_overflow_ceiling: false }
    }

    pub(super) fn discarded(item: T, why: Discard, class: DispatchClass) -> Self {
        let owed = class.row().owed(why);
        Offer {
            outcome: Outcome::Discarded(Discarded { item, why, owed }),
            crossed_lifetime_cap: false,
            hit_overflow_ceiling: false,
        }
    }
}

/// What the call's worker takes next.
#[derive(Debug, PartialEq, Eq)]
pub enum Popped<T> {
    Item(T, DispatchClass),
    /// The call's release: what is queued behind it belongs to the call's
    /// next queue ([`CallQueue::reopen`]).
    Release(RemovalClass),
}

enum Entry<T> {
    Item(T, DispatchClass),
    Release(RemovalClass),
}

impl<T> Entry<T> {
    fn is_new_call(&self) -> bool {
        matches!(self, Entry::Item(_, class) if class.is_new_call())
    }

    fn room(&self) -> Room {
        match self {
            Entry::Item(_, class) => class.row().room,
            Entry::Release(_) => Room::Always,
        }
    }
}

/// One call's queue.
pub struct CallQueue<T> {
    entries: VecDeque<Entry<T>>,
    limits: Limits,
    /// The class of the release queued on this call, once one is: a later
    /// offer is discarded behind it unless a new call waits there.
    released: Option<RemovalClass>,
    /// A new call waits behind the release, for the call's next queue.
    next_call: bool,
    /// Past-bounds items in the overflow: what the ceiling counts.
    waiting: usize,
    /// Counted offers over the call's life until its release is queued,
    /// whatever became of them.
    offered: u64,
    /// Counted offers behind the release for the call's next queue, whatever
    /// became of them: that queue's count when it opens.
    next_offered: u64,
    /// The offers passed [`NEAR_CAP_PERCENT`] of the lifetime cap, and the
    /// call is counted in the near-cap gauge until its release is taken.
    near_cap: bool,
    /// The lifetime cap was crossed: only rows
    /// [admitted when capped](super::class::Row::admitted_when_capped) still
    /// get in.
    capped: bool,
}

impl<T> CallQueue<T> {
    pub fn new(limits: Limits) -> Self {
        Self {
            entries: VecDeque::new(),
            limits: Limits { depth: limits.depth.max(1), ..limits },
            released: None,
            next_call: false,
            waiting: 0,
            offered: 0,
            next_offered: 0,
            near_cap: false,
            capped: false,
        }
    }

    /// Offer `item` of `class`. In order: a counted class counts toward the
    /// lifetime cap unless the release is queued; behind the release, a new
    /// call and every offer after it wait for the call's next queue; else a
    /// capped call refuses what its row does not admit, and an offer behind
    /// the release is discarded; then the row's room decides.
    pub fn offer(&mut self, item: T, class: DispatchClass, metrics: &B2buaMetrics) -> Offer<T> {
        let row = class.row();
        let crossed = row.counted && self.released.is_none() && self.count(metrics);
        let refused = |item, why| Offer {
            crossed_lifetime_cap: crossed,
            ..Offer::discarded(item, why, class)
        };
        // A new call's discard is counted once, as a refused new call, by
        // the router that answers it.
        let event_drop = !class.is_new_call();
        let next_call = self.released.is_some() && (self.next_call || class.is_new_call());
        if next_call && row.counted {
            self.next_offered += 1;
        }
        if !next_call && self.capped && !row.admitted_when_capped() {
            if event_drop {
                metrics.bump_capped_refusal();
            }
            return refused(item, Discard::Capped);
        }
        if let (false, Some(released)) = (next_call, self.released) {
            if event_drop {
                metrics.bump_release_discard();
            }
            return refused(item, Discard::Released(released));
        }
        if self.entries.len() >= self.limits.depth {
            match row.room {
                Room::Bounded => {
                    if event_drop {
                        metrics.bump_queue_drop();
                    }
                    return refused(item, Discard::QueueFull);
                }
                Room::PastBounds if self.waiting >= self.limits.depth => {
                    metrics.bump_queue_drop();
                    metrics.bump_overflow_refused();
                    return Offer {
                        hit_overflow_ceiling: true,
                        ..refused(item, Discard::QueueFull)
                    };
                }
                Room::PastBounds => self.waiting += 1,
                Room::Always => {}
            }
            Self::entered_overflow(metrics);
        }
        self.next_call |= next_call;
        self.entries.push_back(Entry::Item(item, class));
        Offer::queued(crossed)
    }

    /// Queue the call's release behind every item already queued, or in
    /// front of the first new call queued, which waits behind it with every
    /// item after it. The first release names the removal's class; a later
    /// one is ignored. From then on no offer counts toward the lifetime cap.
    pub fn release(&mut self, class: RemovalClass, metrics: &B2buaMetrics) {
        if self.released.is_some() {
            return;
        }
        self.released = Some(class);
        let (len, depth) = (self.entries.len(), self.limits.depth);
        if len >= depth {
            Self::entered_overflow(metrics);
        }
        match self.entries.iter().position(Entry::is_new_call) {
            None => self.entries.push_back(Entry::Release(class)),
            Some(at) => {
                // The last entry of the queue proper moves into the overflow.
                if at < depth && len >= depth && self.entries[depth - 1].room() == Room::PastBounds
                {
                    self.waiting += 1;
                }
                self.entries.insert(at, Entry::Release(class));
                self.next_call = true;
            }
        }
    }

    /// Open the call's next queue on what waits behind the release its
    /// worker just took: the queue counts toward the lifetime cap from what
    /// was offered to it behind the release, its next counted offer past the
    /// cap crossing it, and takes offers again. `false`, and the queue is left
    /// as it is, when nothing waits there.
    pub fn reopen(&mut self, metrics: &B2buaMetrics) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        debug_assert!(self.next_call, "only a new call and what follows it wait behind a release");
        self.released = None;
        self.next_call = false;
        self.offered = std::mem::take(&mut self.next_offered);
        self.capped = false;
        self.note_near_cap(metrics);
        true
    }

    /// Take the oldest entry.
    pub fn pop(&mut self, metrics: &B2buaMetrics) -> Option<Popped<T>> {
        let front = self.entries.pop_front()?;
        // The overflow's oldest entry, if any, moves into the queue proper.
        if let Some(moved) = self.entries.get(self.limits.depth - 1) {
            metrics.add_overflow_depth(-1);
            if moved.room() == Room::PastBounds {
                self.waiting -= 1;
            }
        }
        Some(match front {
            Entry::Item(item, class) => Popped::Item(item, class),
            Entry::Release(class) => {
                // The call leaves the near-cap gauge with its release: a
                // released queue counts no later offer.
                if std::mem::take(&mut self.near_cap) {
                    metrics.add_calls_near_lifetime_cap(-1);
                }
                Popped::Release(class)
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries waiting, the release included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Past-bounds items in the overflow.
    pub fn waiting_past_bounds(&self) -> usize {
        self.waiting
    }

    /// Count one counted offer; `true` when it is the one that crosses the
    /// lifetime cap.
    fn count(&mut self, metrics: &B2buaMetrics) -> bool {
        self.offered += 1;
        self.note_near_cap(metrics);
        let crossed = self.offered > self.limits.lifetime_cap && !self.capped;
        self.capped |= crossed;
        crossed
    }

    /// Enter the near-cap gauge once the offers pass [`NEAR_CAP_PERCENT`] of
    /// the lifetime cap.
    fn note_near_cap(&mut self, metrics: &B2buaMetrics) {
        let cap = self.limits.lifetime_cap as u128;
        if !self.near_cap && self.offered as u128 * 100 > cap * NEAR_CAP_PERCENT as u128 {
            self.near_cap = true;
            metrics.add_calls_near_lifetime_cap(1);
        }
    }

    fn entered_overflow(metrics: &B2buaMetrics) {
        metrics.bump_past_bound(PastBound::Depth);
        metrics.add_overflow_depth(1);
    }
}
