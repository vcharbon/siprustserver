//! [`Durations`]: the cells of one histogram of durations, its fixed buckets
//! stated by its family's renderer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use metric_catalogue::{Family, HistogramValue};

use super::cells;

/// `N` cumulative counts by `le` in seconds, the sum in milliseconds and the
/// observation count.
#[derive(Debug)]
pub(super) struct Durations<const N: usize> {
    buckets: [AtomicU64; N],
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl<const N: usize> Default for Durations<N> {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl<const N: usize> Durations<N> {
    /// Count `elapsed` in every bucket of `bounds` at or above it.
    pub(super) fn observe(&self, bounds: &[f64; N], elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        let buckets = bounds.iter().zip(&self.buckets);
        buckets.filter(|(le, _)| secs <= **le).for_each(|(_, n)| cells::add(n, 1));
        cells::add(&self.sum_ms, sip_clock::wall::millis_u64(elapsed));
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Write `family` under the buckets of `bounds`.
    pub(super) fn render(&self, family: &Family, bounds: &[f64; N], s: &mut String) {
        family.render_histogram(s, |_| HistogramValue {
            buckets: bounds
                .iter()
                .zip(&self.buckets)
                .map(|(le, n)| (*le, n.load(Ordering::Relaxed)))
                .collect(),
            sum: self.sum_ms.load(Ordering::Relaxed) as f64 / 1_000.0,
            count: self.count.load(Ordering::Relaxed),
        });
    }
}
