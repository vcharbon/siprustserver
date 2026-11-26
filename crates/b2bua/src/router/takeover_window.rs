//! Whether this node may be taking a peer's calls over (ADR-0014, ADR-0047),
//! read with no lock on every request naming no call: while it may, a
//! request a replica could still name is answered through its transaction
//! (`router::unroutable`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::repl::DepartedCount;
use crate::store::CallState;

/// The two marks of a takeover in progress, each an atomic.
pub struct TakeoverWindow {
    copies: Arc<AtomicUsize>,
    departed: Option<DepartedCount>,
}

impl TakeoverWindow {
    /// The window over `state`'s takeover copies and the `departed` peers of
    /// the node's supervisor (`None` on an unwired node).
    pub fn new(state: &CallState, departed: Option<DepartedCount>) -> Self {
        Self { copies: state.takeover_copies(), departed }
    }

    /// Whether a takeover may be in progress: a peer departed and has not
    /// rejoined, so its replicas here may yet be served, or this node serves
    /// a live takeover copy.
    pub fn open(&self) -> bool {
        self.copies.load(Ordering::Acquire) != 0
            || self.departed.as_ref().is_some_and(DepartedCount::any)
    }
}
