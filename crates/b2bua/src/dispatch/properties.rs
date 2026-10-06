//! Properties of [`CallQueue`] over arbitrary interleavings of offers of
//! every dispatch class, pops and a release, checked against a model of the
//! queue's entries; a release taken with a new call behind it reopens the
//! queue, as its worker does. The model reads each class's room, counting and owed
//! answers from the literal table ([`super::expected`]), never from the
//! implementation's rows.

use proptest::prelude::*;

use super::class::{DispatchClass, Room};
use super::expected::expected;
use super::queue::{CallQueue, Discard, Limits, Outcome, Popped};
use crate::metrics::{B2buaMetrics, RemovalClass};

#[derive(Debug, Clone, Copy)]
enum Op {
    Offer(DispatchClass),
    Pop,
    Release(RemovalClass),
}

fn class() -> impl Strategy<Value = DispatchClass> {
    proptest::sample::select(DispatchClass::ALL.to_vec())
}

fn removal() -> impl Strategy<Value = RemovalClass> {
    proptest::sample::select(vec![
        RemovalClass::Terminated,
        RemovalClass::SelfRelease,
        RemovalClass::Orphan,
    ])
}

/// Offers and pops, with a release at most once, late in the sequence so
/// the queue fills, overflows and drains under mixed classes before it.
fn ops() -> impl Strategy<Value = Vec<Op>> {
    let step = prop_oneof![2 => class().prop_map(Op::Offer), 1 => Just(Op::Pop)];
    (
        proptest::collection::vec(step, 0..120),
        proptest::option::weighted(0.7, (50u32..=100, removal())),
    )
        .prop_map(|(mut ops, release)| {
            if let Some((percent, class)) = release {
                let at = ops.len() * percent as usize / 100;
                ops.insert(at, Op::Release(class));
            }
            ops
        })
}

fn limits() -> impl Strategy<Value = Limits> {
    (1usize..5, prop_oneof![1u64..25, Just(u64::MAX)])
        .prop_map(|(depth, lifetime_cap)| Limits { depth, lifetime_cap })
}

/// The model's view of one queued entry: an offered item's id and class, or
/// the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Item(u32, DispatchClass),
    Release(RemovalClass),
}

/// What the model tracks of a queue.
#[derive(Default)]
struct Model {
    entries: std::collections::VecDeque<Entry>,
    released: Option<RemovalClass>,
    /// A new call waits behind the release.
    next_call: bool,
    /// The release cut in front of a queued new call: one past-bounds item
    /// may sit past the overflow ceiling.
    cut: bool,
    counted: u64,
    /// Counted offers behind the release for the next queue.
    next_counted: u64,
    /// The lifetime cap was crossed on this queue.
    capped: bool,
    ran: Vec<u32>,
    /// Every discarded item, why, and whether its class is a new call's
    /// (counted by the router, not as a dispatch drop).
    discarded: Vec<(u32, Discard, bool)>,
    queued: Vec<u32>,
    crossings: u32,
    release_taken: bool,
}

impl Model {
    /// Past-bounds items past the queue proper.
    fn past_bounds_in_overflow(&self, depth: usize) -> usize {
        self.entries
            .iter()
            .skip(depth)
            .filter(|e| matches!(e, Entry::Item(_, c) if expected(*c).room == Room::PastBounds))
            .count()
    }

    /// The near-cap gauge this queue holds.
    fn near_cap_gauge(&self, cap: u64) -> i64 {
        let near = self.counted as u128 * 100 > cap as u128 * 80;
        i64::from(near && !self.release_taken)
    }
}

/// Run `ops` on a queue with `limits`, checking every step, then drain it.
fn run(limits: Limits, ops: &[Op]) -> Result<(), TestCaseError> {
    let metrics = B2buaMetrics::new();
    let mut q = CallQueue::<u32>::new(limits);
    let mut m = Model::default();
    let depth = limits.depth;
    let mut next_id = 0u32;
    for op in ops {
        match *op {
            Op::Offer(class) => {
                let id = next_id;
                next_id += 1;
                let e = expected(class);
                let len_before = m.entries.len();
                let overflow_full = m.past_bounds_in_overflow(depth) >= depth;
                // A released queue counts nothing toward the lifetime cap;
                // the first counted offer past it crosses it.
                let mut crosses = false;
                if e.counted && m.released.is_none() {
                    m.counted += 1;
                    crosses = m.counted > limits.lifetime_cap && !m.capped;
                    m.capped |= crosses;
                }
                // Behind the release, a new call and what follows it wait for
                // the next queue, counted toward it.
                let next = m.released.is_some() && (m.next_call || class.is_new_call());
                if next && e.counted {
                    m.next_counted += 1;
                }
                let capped = !next && m.capped;
                let offer = q.offer(id, class, &metrics);
                prop_assert_eq!(offer.crossed_lifetime_cap, crosses);
                if crosses {
                    m.crossings += 1;
                }
                match offer.outcome {
                    Outcome::Queued => {
                        prop_assert!(!offer.hit_overflow_ceiling);
                        // A capped call admits exactly its rows.
                        prop_assert!(!capped || e.admitted_when_capped(), "{class:?}");
                        prop_assert!(
                            m.released.is_none() || next,
                            "only a new call and what follows it queue behind a release"
                        );
                        prop_assert!(len_before < depth || e.room != Room::Bounded);
                        prop_assert!(
                            len_before < depth || e.room != Room::PastBounds || !overflow_full
                        );
                        m.next_call |= next;
                        m.entries.push_back(Entry::Item(id, class));
                        m.queued.push(id);
                    }
                    Outcome::Discarded(d) => {
                        prop_assert_eq!(d.item, id, "the discarded item is handed back");
                        let why = if capped && !e.admitted_when_capped() {
                            Discard::Capped
                        } else if let (false, Some(class)) = (next, m.released) {
                            Discard::Released(class)
                        } else {
                            prop_assert!(len_before >= depth, "{class:?} refused with room");
                            prop_assert!(e.room != Room::Always, "always-room refused");
                            prop_assert!(e.room == Room::Bounded || overflow_full);
                            Discard::QueueFull
                        };
                        prop_assert_eq!(d.why, why, "{:?}", class);
                        prop_assert_eq!(d.owed, e.owed(why), "the table's owed answer");
                        prop_assert_eq!(
                            offer.hit_overflow_ceiling,
                            why == Discard::QueueFull && e.room == Room::PastBounds
                        );
                        // Always-room items are lost only behind a release.
                        if e.room == Room::Always {
                            prop_assert!(matches!(why, Discard::Released(_)));
                        }
                        m.discarded.push((id, why, class.is_new_call()));
                    }
                }
            }
            Op::Pop => pop(&mut q, &mut m, &metrics)?,
            Op::Release(class) => {
                q.release(class, &metrics);
                if m.released.is_none() {
                    m.released = Some(class);
                    let first_new_call = m
                        .entries
                        .iter()
                        .position(|e| matches!(e, Entry::Item(_, c) if c.is_new_call()));
                    match first_new_call {
                        Some(at) => {
                            m.entries.insert(at, Entry::Release(class));
                            m.next_call = true;
                            m.cut = true;
                        }
                        None => m.entries.push_back(Entry::Release(class)),
                    }
                }
            }
        }
        // The overflow never holds more past-bounds items than its ceiling,
        // but for the one a release cutting in moved there.
        prop_assert!(m.past_bounds_in_overflow(depth) <= depth + usize::from(m.cut));
        prop_assert_eq!(q.waiting_past_bounds(), m.past_bounds_in_overflow(depth));
        prop_assert_eq!(q.len(), m.entries.len());
        prop_assert_eq!(metrics.overflow_depth(), m.entries.len().saturating_sub(depth) as i64);
        prop_assert_eq!(metrics.calls_near_lifetime_cap(), m.near_cap_gauge(limits.lifetime_cap));
    }
    prop_assert!(m.crossings <= 1, "the lifetime cap is crossed once per queue");

    // Drain: every queued item runs, in offer order.
    while !m.entries.is_empty() {
        pop(&mut q, &mut m, &metrics)?;
    }
    prop_assert!(q.pop(&metrics).is_none());
    prop_assert_eq!(&m.ran, &m.queued, "FIFO: items run in the order they were queued");
    // Every offer ends exactly once: ran, or discarded with a stated reason.
    let mut ended: Vec<u32> =
        m.ran.iter().copied().chain(m.discarded.iter().map(|d| d.0)).collect();
    ended.sort_unstable();
    prop_assert_eq!(ended, (0..next_id).collect::<Vec<_>>());
    prop_assert_eq!(metrics.overflow_depth(), 0);
    prop_assert_eq!(metrics.calls_near_lifetime_cap(), m.near_cap_gauge(limits.lifetime_cap));

    let count =
        |f: fn(&Discard) -> bool| m.discarded.iter().filter(|d| !d.2 && f(&d.1)).count() as u64;
    prop_assert_eq!(metrics.queue_drops_total(), count(|w| *w == Discard::QueueFull));
    prop_assert_eq!(metrics.release_discards_total(), count(|w| matches!(w, Discard::Released(_))));
    prop_assert_eq!(metrics.capped_refusals_total(), count(|w| *w == Discard::Capped));
    Ok(())
}

/// Pop one entry: the model's oldest, item and class or release alike.
fn pop(q: &mut CallQueue<u32>, m: &mut Model, metrics: &B2buaMetrics) -> Result<(), TestCaseError> {
    let expected = m.entries.pop_front();
    match (q.pop(metrics), expected) {
        (None, None) => {}
        (Some(Popped::Item(id, class)), Some(Entry::Item(want, want_class))) => {
            prop_assert_eq!((id, class), (want, want_class));
            m.ran.push(id);
        }
        (Some(Popped::Release(class)), Some(Entry::Release(want))) => {
            prop_assert_eq!(class, want);
            prop_assert_eq!(m.next_call, !m.entries.is_empty(), "only a new call waits behind");
            // The worker reopens the queue on what waits behind the release:
            // a fresh queue, counting afresh.
            prop_assert_eq!(q.reopen(metrics), m.next_call);
            if m.next_call {
                m.released = None;
                m.next_call = false;
                m.counted = std::mem::take(&mut m.next_counted);
                m.capped = false;
                m.crossings = 0;
            } else {
                m.release_taken = true;
            }
        }
        (got, want) => prop_assert!(false, "popped {got:?}, the model holds {want:?}"),
    }
    Ok(())
}

proptest! {
    // A failing case is reported, never persisted into the tree.
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::with_cases(512) })]

    /// FIFO per call across every room; the overflow never exceeds its
    /// ceiling but for one item a release cutting in moved; every offer ends
    /// exactly once as run or discarded with its reason and its table row's
    /// owed answer; always-room items are lost only behind a release with no
    /// new call waiting; a capped call admits exactly the rows that keep
    /// their room past the cap; a released queue counts nothing.
    #[test]
    fn a_call_queue_keeps_its_contract(limits in limits(), ops in ops()) {
        run(limits, &ops)?;
    }

    /// The same contract on a queue that is never drained before its end:
    /// floods meet a full queue, a full overflow and the lifetime cap.
    #[test]
    fn a_stalled_call_queue_keeps_its_contract(
        limits in limits(),
        classes in proptest::collection::vec(class(), 0..120),
        release_at in proptest::option::of(0usize..120),
    ) {
        let mut ops: Vec<Op> = classes.into_iter().map(Op::Offer).collect();
        if let Some(at) = release_at {
            ops.insert(at.min(ops.len()), Op::Release(RemovalClass::Terminated));
        }
        run(limits, &ops)?;
    }
}

/// A past-bounds item finding the overflow at its ceiling says so; the
/// node's own work still queues past it.
#[test]
fn the_overflow_ceiling_is_hit_by_past_bounds_items_only() {
    let metrics = B2buaMetrics::new();
    let mut q = CallQueue::new(Limits { depth: 2, lifetime_cap: u64::MAX });
    for i in 0..4 {
        assert!(matches!(q.offer(i, DispatchClass::Cancelled, &metrics).outcome, Outcome::Queued));
    }
    let refused = q.offer(4, DispatchClass::Cancelled, &metrics);
    assert!(refused.hit_overflow_ceiling);
    assert_eq!(metrics.overflow_refused_total(), 1);
    let own = q.offer(5, DispatchClass::Timer, &metrics);
    assert!(matches!(own.outcome, Outcome::Queued) && !own.hit_overflow_ceiling);
    assert_eq!(metrics.past_bound_of_total(super::PastBound::Depth), 3);
}

/// Once its release is queued, a queue counts no offer toward its lifetime
/// cap: an offer reaching it after its worker took the release — the slot
/// cloned before the worker removed it — moves no gauge and crosses nothing.
/// The near-cap gauge is settled when the release is taken.
#[test]
fn a_released_queue_counts_no_offer_toward_its_lifetime_cap() {
    let metrics = B2buaMetrics::new();
    let mut q = CallQueue::new(Limits { depth: 64, lifetime_cap: 10 });
    for i in 0..8 {
        assert!(matches!(
            q.offer(i, DispatchClass::OtherRequest, &metrics).outcome,
            Outcome::Queued
        ));
    }
    q.release(RemovalClass::Terminated, &metrics);
    while q.pop(&metrics).is_some() {}
    assert_eq!(metrics.calls_near_lifetime_cap(), 0);
    for i in 8..12 {
        let late = q.offer(i, DispatchClass::OtherRequest, &metrics);
        assert!(!late.crossed_lifetime_cap, "a released queue crosses nothing");
        assert!(matches!(
            late.outcome,
            Outcome::Discarded(super::Discarded {
                why: Discard::Released(RemovalClass::Terminated),
                ..
            })
        ));
    }
    assert_eq!(metrics.calls_near_lifetime_cap(), 0, "no gauge left behind");
}

/// A queue near its lifetime cap leaves the near-cap gauge when its release
/// is taken.
#[test]
fn a_queue_near_its_lifetime_cap_leaves_the_gauge_when_its_release_is_taken() {
    let metrics = B2buaMetrics::new();
    let mut q = CallQueue::new(Limits { depth: 64, lifetime_cap: 10 });
    for i in 0..9 {
        let _ = q.offer(i, DispatchClass::OtherRequest, &metrics);
    }
    assert_eq!(metrics.calls_near_lifetime_cap(), 1);
    q.release(RemovalClass::Terminated, &metrics);
    while let Some(popped) = q.pop(&metrics) {
        if matches!(popped, Popped::Release(_)) {
            assert_eq!(metrics.calls_near_lifetime_cap(), 0, "settled with the release");
        }
    }
}

/// What waits behind a release for the call's next queue counts toward that
/// queue's lifetime cap: the next queue's first offer past the cap crosses it.
#[test]
fn offers_waiting_for_the_next_queue_count_toward_its_lifetime_cap() {
    let metrics = B2buaMetrics::new();
    let mut q = CallQueue::new(Limits { depth: 64, lifetime_cap: 2 });
    assert!(matches!(q.offer(0, DispatchClass::OtherRequest, &metrics).outcome, Outcome::Queued));
    q.release(RemovalClass::Terminated, &metrics);
    for (id, class) in [(1, DispatchClass::InitialInvite), (2, DispatchClass::OtherRequest)] {
        let next = q.offer(id, class, &metrics);
        assert!(matches!(next.outcome, Outcome::Queued) && !next.crossed_lifetime_cap);
    }
    assert!(matches!(q.pop(&metrics), Some(Popped::Item(0, _))));
    assert!(matches!(q.pop(&metrics), Some(Popped::Release(_))));
    assert!(q.reopen(&metrics));
    let third = q.offer(3, DispatchClass::OtherRequest, &metrics);
    assert!(
        third.crossed_lifetime_cap,
        "the next queue's third counted offer crosses its cap of 2"
    );
}
