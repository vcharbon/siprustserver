//! `EventSequencer` — monotonic per-scenario counter (port of
//! `src/test-harness/framework/EventSequencer.ts`).
//!
//! A deterministic tiebreaker when two recorded events share the same `at_ms`
//! timestamp. Every recording channel allocates its `seq` from one shared
//! counter so a renderer can sort the merged trace by `(at_ms, seq)` and get
//! the order events were actually captured in — `at_ms` collisions are common
//! under a paused test clock.
//!
//! Where the TS source split `next` (Effect) from `nextSync` (raw callback),
//! recording in Rust is plain synchronous, so a single [`EventSequencer::next`]
//! suffices.

use std::sync::atomic::{AtomicU64, Ordering};

/// A shared monotonic counter. Cheap to clone — every clone draws from the
/// same atomic, so cloning the handle into each channel keeps one global
/// order per scenario.
#[derive(Debug, Default)]
pub struct EventSequencer {
    counter: AtomicU64,
}

impl EventSequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next strictly-increasing sequence number (starts at 1).
    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }
}
