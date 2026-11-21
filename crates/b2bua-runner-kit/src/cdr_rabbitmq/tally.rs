//! The one count of each record's end: `b2bua_cdr_written_total` or
//! `b2bua_cdr_dropped_total{reason}` in the shared registry, and the writer's
//! observer told the same end with the time since the writer was handed the
//! record.

use std::sync::Arc;

use b2bua::cdr::{CdrDelivery, CdrDeliveryObserver, CdrDrop};
use b2bua::metrics::B2buaMetrics;
use sip_clock::wall::millis_u64;
use tokio::time::Instant;

use super::nack_log::{NackLog, NACK_LOG_INTERVAL};

/// Counts the ends of the records one writer was handed.
#[derive(Clone)]
pub(super) struct Tally {
    metrics: B2buaMetrics,
    observer: Option<Arc<dyn CdrDeliveryObserver>>,
    /// The writer's nacks, every session's, and their rate-limited warning.
    nacks: Arc<NackLog>,
}

impl Tally {
    /// A tally counting into `metrics`, with no observer.
    pub(super) fn new(metrics: B2buaMetrics) -> Self {
        Self { metrics, observer: None, nacks: Arc::new(NackLog::new()) }
    }

    /// Tells `observer` every end counted from here on.
    pub(super) fn observed_by(&mut self, observer: Arc<dyn CdrDeliveryObserver>) {
        self.observer = Some(observer);
    }

    /// The registry the tally counts into, where the writer counts its link.
    pub(super) fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    /// The record handed at `handed_at` was acked by the broker. The observer
    /// is told before the registry counts it.
    pub(super) fn written(&self, handed_at: Instant) {
        self.tell(CdrDelivery::Written, handed_at);
        self.metrics.bump_cdr_written();
    }

    /// The record handed at `handed_at` ended without the broker's ack, for
    /// `why`. The observer is told before the registry counts it.
    pub(super) fn dropped(&self, handed_at: Instant, why: CdrDrop) {
        self.tell(CdrDelivery::Dropped, handed_at);
        self.metrics.bump_cdr_dropped(why);
    }

    /// The record handed at `handed_at` was nacked: dropped for
    /// [`CdrDrop::Nacked`], after the warning when one is due
    /// ([`NackLog::nacked`]).
    pub(super) fn nacked(&self, handed_at: Instant) {
        if let Some(nacked) = self.nacks.nacked() {
            tracing::warn!(
                nacked,
                interval_ms = millis_u64(NACK_LOG_INTERVAL),
                "CDR publishes nacked by the broker since the last such line, each dropped; \
                 said at most once per interval"
            );
        }
        self.dropped(handed_at, CdrDrop::Nacked);
    }

    fn tell(&self, delivery: CdrDelivery, handed_at: Instant) {
        if let Some(observer) = &self.observer {
            observer.ended(delivery, handed_at.elapsed());
        }
    }
}
