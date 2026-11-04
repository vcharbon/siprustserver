//! [`EventAges`]: the age of the oldest event waiting in the layer's event
//! queue, read at any instant. The one producer (the owner) stamps each event
//! into a ring slot by its place in the order of entry and counts it in; the
//! one consumer counts each event it takes. The oldest waiting event is the
//! first one counted in and not yet taken, and its stamp is still in its
//! slot: the ring holds twice the queue's bound, more than ever wait at once.
//! An enqueue is one clock read and two atomic stores; a take, one atomic
//! store; no lock.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::time::Duration;

use tokio::time::Instant;

/// The entry stamps of the events in the queue.
pub(crate) struct EventAges {
    base: Instant,
    ring: Box<[AtomicU64]>,
    mask: u64,
    /// Events counted into the queue.
    entered: AtomicU64,
    /// Events taken from it.
    taken: AtomicU64,
}

impl EventAges {
    /// The stamps of a queue bounded at `capacity`.
    pub(crate) fn new(capacity: usize) -> Self {
        let slots = capacity.saturating_mul(2).max(2).next_power_of_two();
        let ring = (0..slots).map(|_| AtomicU64::new(0)).collect();
        let mask = u64::try_from(slots - 1).unwrap_or(u64::MAX);
        Self {
            base: Instant::now(),
            ring,
            mask,
            entered: AtomicU64::new(0),
            taken: AtomicU64::new(0),
        }
    }

    /// Stamp the next event to enter, now; returns its place, which
    /// [`entered`](Self::entered) counts in once the queue took it. Producer
    /// only.
    pub(crate) fn stamp(&self) -> u64 {
        let place = self.entered.load(Relaxed);
        if let Some(slot) = self.slot(place) {
            slot.store(self.now(), Relaxed);
        }
        place
    }

    /// The event stamped at `place` is in the queue. Producer only.
    pub(crate) fn entered(&self, place: u64) {
        self.entered.store(place + 1, Release);
    }

    /// One event left the queue. Consumer only.
    pub(crate) fn taken(&self) {
        let taken = self.taken.load(Relaxed);
        self.taken.store(taken + 1, Release);
    }

    /// How long the oldest event in the queue has waited; zero when none
    /// waits.
    pub(crate) fn oldest(&self) -> Duration {
        let taken = self.taken.load(Acquire);
        if taken >= self.entered.load(Acquire) {
            return Duration::ZERO;
        }
        let stamp = self.slot(taken).map_or(0, |slot| slot.load(Relaxed));
        Duration::from_nanos(self.now().saturating_sub(stamp))
    }

    fn slot(&self, place: u64) -> Option<&AtomicU64> {
        usize::try_from(place & self.mask).ok().and_then(|i| self.ring.get(i))
    }

    fn now(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[tokio::test(start_paused = true)]
    async fn the_oldest_waiting_event_ages_while_nothing_is_taken() {
        let ages = EventAges::new(4);
        assert_eq!(ages.oldest(), Duration::ZERO, "an empty queue");
        let first = ages.stamp();
        ages.entered(first);
        tokio::time::advance(100 * MS).await;
        let second = ages.stamp();
        ages.entered(second);
        tokio::time::advance(900 * MS).await;
        assert_eq!(ages.oldest(), 1000 * MS, "the first has waited a second");
        ages.taken();
        assert_eq!(ages.oldest(), 900 * MS, "then the second");
        ages.taken();
        assert_eq!(ages.oldest(), Duration::ZERO, "none waits");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stamp_not_counted_in_is_not_read() {
        let ages = EventAges::new(4);
        let _refused = ages.stamp();
        tokio::time::advance(100 * MS).await;
        assert_eq!(ages.oldest(), Duration::ZERO, "the queue did not take it");
        let place = ages.stamp();
        ages.entered(place);
        tokio::time::advance(10 * MS).await;
        assert_eq!(ages.oldest(), 10 * MS, "its slot holds the event that entered");
    }

    #[tokio::test(start_paused = true)]
    async fn the_ring_wraps_without_losing_the_head() {
        let ages = EventAges::new(2);
        for round in 0..10u32 {
            for _ in 0..2 {
                let place = ages.stamp();
                ages.entered(place);
            }
            tokio::time::advance(MS * (round + 1)).await;
            assert_eq!(ages.oldest(), MS * (round + 1), "round {round}");
            ages.taken();
            ages.taken();
        }
    }
}
