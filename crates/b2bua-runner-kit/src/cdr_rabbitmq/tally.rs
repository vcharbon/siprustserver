//! The one count of each record's end: `cdr_written_total` or
//! `cdr_dropped_total` in the shared registry, and the writer's observer told
//! the same end with the time since the writer was handed the record.

use std::sync::Arc;

use b2bua::cdr::{CdrDelivery, CdrDeliveryObserver};
use b2bua::metrics::B2buaMetrics;
use tokio::time::Instant;

/// Counts the ends of the records one writer was handed.
#[derive(Clone)]
pub(super) struct Tally {
    metrics: B2buaMetrics,
    observer: Option<Arc<dyn CdrDeliveryObserver>>,
}

impl Tally {
    /// A tally counting into `metrics`, with no observer.
    pub(super) fn new(metrics: B2buaMetrics) -> Self {
        Self { metrics, observer: None }
    }

    /// Tells `observer` every end counted from here on.
    pub(super) fn observed_by(&mut self, observer: Arc<dyn CdrDeliveryObserver>) {
        self.observer = Some(observer);
    }

    /// The record handed at `handed_at` was acked by the broker.
    pub(super) fn written(&self, handed_at: Instant) {
        self.metrics.bump_cdr_written();
        self.tell(CdrDelivery::Written, handed_at);
    }

    /// The record handed at `handed_at` ended without the broker's ack.
    pub(super) fn dropped(&self, handed_at: Instant) {
        self.metrics.bump_cdr_dropped();
        self.tell(CdrDelivery::Dropped, handed_at);
    }

    fn tell(&self, delivery: CdrDelivery, handed_at: Instant) {
        if let Some(observer) = &self.observer {
            observer.ended(delivery, handed_at.elapsed());
        }
    }
}
