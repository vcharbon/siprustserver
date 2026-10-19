//! Relaxed counter cells and their rows: reads and writes that name a cell
//! past the last read zero and write nothing.

use std::sync::atomic::{AtomicU64, Ordering};

/// Add `n` to `a`.
pub(super) fn add(a: &AtomicU64, n: u64) {
    a.fetch_add(n, Ordering::Relaxed);
}

/// The count `a` holds.
pub(super) fn get(a: &AtomicU64) -> u64 {
    a.load(Ordering::Relaxed)
}

/// Add `n` to the cell `at` of `cells`.
pub(super) fn add_at(cells: &[AtomicU64], at: usize, n: u64) {
    if let Some(a) = cells.get(at) {
        add(a, n);
    }
}

/// The cell `at` of `cells`.
pub(super) fn get_at(cells: &[AtomicU64], at: usize) -> u64 {
    cells.get(at).map_or(0, get)
}

/// `total` of the value `at` of `all`.
pub(super) fn total<T: Copy>(all: &[T], at: usize, total: impl Fn(T) -> u64) -> u64 {
    all.get(at).copied().map_or(0, total)
}

/// The row `at` of `rows`, empty past the last.
pub(super) fn row<const N: usize>(rows: &[[AtomicU64; N]], at: usize) -> &[AtomicU64] {
    rows.get(at).map_or(&[], |r| r.as_slice())
}
