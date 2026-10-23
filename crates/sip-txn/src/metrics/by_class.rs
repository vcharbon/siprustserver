//! One counter per class of an enumerated label, at the class's position.

use std::sync::atomic::{AtomicU64, Ordering};

/// `N` counters, one per class of a label whose classes number `N`; a class
/// names its counter by its position in the label's `ALL`.
#[derive(Debug)]
pub(crate) struct ByClass<const N: usize>([AtomicU64; N]);

impl<const N: usize> ByClass<N> {
    pub(crate) fn new() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }

    /// Count one for the class at `position`.
    pub(crate) fn add(&self, position: usize) {
        if let Some(cell) = self.0.get(position) {
            cell.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The count of the class at `position`; a position no class holds has
    /// counted nothing.
    pub(crate) fn get(&self, position: usize) -> u64 {
        self.0.get(position).map_or(0, |cell| cell.load(Ordering::Relaxed))
    }
}
