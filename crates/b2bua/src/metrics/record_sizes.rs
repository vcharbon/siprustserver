//! The size of each call record the record-size cap held (ADR-0057 §9), the
//! histogram `b2bua_call_record_bytes`.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_catalogue::HistogramValue;

use super::catalogue::caps::{CALL_RECORD_BYTES, RECORD_BYTES_BUCKETS};
use super::{cells, B2buaMetrics};

/// The cumulative counts by `le` in bytes, the sum and the observation count.
#[derive(Debug)]
pub(super) struct RecordSizes {
    buckets: [AtomicU64; RECORD_BYTES_BUCKETS.len()],
    sum: AtomicU64,
    count: AtomicU64,
}

impl Default for RecordSizes {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl RecordSizes {
    /// Write the histogram.
    pub(super) fn render(&self, s: &mut String) {
        CALL_RECORD_BYTES.render_histogram(s, |_| HistogramValue {
            buckets: RECORD_BYTES_BUCKETS
                .iter()
                .zip(&self.buckets)
                .map(|(le, n)| (*le, cells::get(n)))
                .collect(),
            sum: self.sum.load(Ordering::Relaxed) as f64,
            count: self.count.load(Ordering::Relaxed),
        });
    }
}

impl B2buaMetrics {
    /// A held write's record encoded to a body of `bytes`.
    pub fn observe_call_record_bytes(&self, bytes: u64) {
        let sizes = &self.inner.record_sizes;
        let at_or_above = RECORD_BYTES_BUCKETS.iter().zip(&sizes.buckets);
        at_or_above.filter(|(le, _)| bytes as f64 <= **le).for_each(|(_, n)| cells::add(n, 1));
        cells::add(&sizes.sum, bytes);
        cells::add(&sizes.count, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The histogram reads from the first scrape, then counts each record in
    /// every bucket at or above its size.
    #[test]
    fn each_record_is_counted_at_and_above_its_size() {
        let metrics = B2buaMetrics::new();
        assert!(metrics.prometheus_text().contains("b2bua_call_record_bytes_count 0"));
        metrics.observe_call_record_bytes(6_000);
        metrics.observe_call_record_bytes(2_000_000);
        let text = metrics.prometheus_text();
        assert!(text.contains("b2bua_call_record_bytes_bucket{le=\"4096\"} 0"), "{text}");
        assert!(text.contains("b2bua_call_record_bytes_bucket{le=\"16384\"} 1"), "{text}");
        assert!(text.contains("b2bua_call_record_bytes_bucket{le=\"1048576\"} 1"), "{text}");
        assert!(text.contains("b2bua_call_record_bytes_bucket{le=\"4194304\"} 2"), "{text}");
        assert!(text.contains("b2bua_call_record_bytes_sum 2006000"), "{text}");
        assert!(text.contains("b2bua_call_record_bytes_count 2"), "{text}");
    }
}
