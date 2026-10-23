//! A fixed table of counters, one slot per label value, read and bumped
//! through checked access.

use std::sync::atomic::{AtomicU64, Ordering};

/// Add one to the counter at `slot` of `counters`.
pub(super) fn bump(counters: &[AtomicU64], slot: usize) {
    if let Some(counter) = counters.get(slot) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The counter at `slot` of `counters`; a slot past them counted nothing.
pub(super) fn read(counters: &[AtomicU64], slot: usize) -> u64 {
    counters.get(slot).map_or(0, |c| c.load(Ordering::Relaxed))
}
