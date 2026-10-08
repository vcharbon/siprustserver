//! A fixed-bucket latency histogram: bounded memory, O(1) record, approximate
//! quantiles (a bucket's upper bound) for the on-disk report, and the
//! Prometheus histogram value a scrape renders.

use metric_catalogue::HistogramValue;

/// A log-bucket histogram of millisecond values. Its bounds are fixed at
/// construction ([`call`](Self::call) or [`round_trip`](Self::round_trip));
/// a value past the last bound counts only in `+Inf`.
#[derive(Clone)]
pub(crate) struct Hist {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    pub(crate) total: u64,
    sum: f64,
    pub(crate) max: f64,
}

impl Hist {
    /// A whole call or a checkpoint from call start: 48 bounds from 0.1 ms in
    /// ×1.4 steps (to ≈730 s), sized for scripted hold times.
    pub(crate) fn call() -> Self {
        Self::geometric(48, 1.4)
    }

    /// One SIP round trip: 60 bounds from 0.1 ms in ×1.25 steps (to ≈52 s),
    /// so sub-millisecond to 100 ms exchanges land in buckets 25 % apart.
    pub(crate) fn round_trip() -> Self {
        Self::geometric(60, 1.25)
    }

    fn geometric(n: i32, ratio: f64) -> Self {
        let bounds: Vec<f64> = (0..n).map(|i| 0.1 * ratio.powi(i)).collect();
        let counts = vec![0u64; bounds.len() + 1];
        Self { bounds, counts, total: 0, sum: 0.0, max: 0.0 }
    }

    pub(crate) fn record(&mut self, ms: f64) {
        let idx = self.bounds.partition_point(|b| *b < ms);
        self.counts[idx] += 1;
        self.total += 1;
        self.sum += ms;
        if ms > self.max {
            self.max = ms;
        }
    }

    /// Approximate quantile in milliseconds (the upper bound of the bucket the
    /// q-th value falls in; `max` for the overflow bucket).
    pub(crate) fn quantile_ms(&self, q: f64) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let target = (q * self.total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            cum += c;
            if cum >= target {
                return self
                    .bounds
                    .get(i)
                    .copied()
                    .unwrap_or(self.max)
                    .min(self.max.max(0.0))
                    .max(0.0);
            }
        }
        self.max
    }

    /// This histogram as one Prometheus histogram series: cumulative counts
    /// at every bound (seconds), the sum (seconds) and the count. A bound's
    /// bucket holds the values at or under it, as `record` files them.
    pub(crate) fn histogram_value(&self) -> HistogramValue {
        let mut cum = 0u64;
        let buckets = self
            .bounds
            .iter()
            .zip(&self.counts)
            .map(|(bound_ms, n)| {
                cum += n;
                ((bound_ms * 1e6).round() / 1e9, cum)
            })
            .collect();
        HistogramValue { buckets, sum: (self.sum * 1e6).round() / 1e9, count: self.total }
    }

    pub(crate) fn mean_ms(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.sum / self.total as f64
        }
    }
}
