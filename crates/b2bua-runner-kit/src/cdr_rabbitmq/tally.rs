//! The one count of each record's end: `b2bua_cdr_written_total` or
//! `b2bua_cdr_dropped_total{reason}` in the shared registry, and the writer's
//! observer told the same end with the time since the writer was handed the
//! record. An observer that panics costs neither the count nor its caller.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use b2bua::cdr::{CdrDelivery, CdrDeliveryObserver, CdrDrop, LineRate, LINE_INTERVAL};
use b2bua::metrics::B2buaMetrics;
use sip_clock::wall::millis_u64;
use tokio::time::Instant;

/// Counts the ends of the records one writer was handed.
#[derive(Clone)]
pub(super) struct Tally {
    metrics: B2buaMetrics,
    observer: Option<Arc<dyn CdrDeliveryObserver>>,
    /// The writer's nacks, every session's, and the rate of their warning.
    nacks: Arc<LineRate>,
    /// The observer's panics and the rate of their error line.
    observer_panics: Arc<LineRate>,
}

impl Tally {
    /// A tally counting into `metrics`, with no observer.
    pub(super) fn new(metrics: B2buaMetrics) -> Self {
        Self {
            metrics,
            observer: None,
            nacks: Arc::new(LineRate::new()),
            observer_panics: Arc::new(LineRate::new()),
        }
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
    /// ([`LineRate::counted`]).
    pub(super) fn nacked(&self, handed_at: Instant) {
        if let Some(nacked) = self.nacks.counted() {
            tracing::warn!(
                nacked,
                interval_ms = millis_u64(LINE_INTERVAL),
                "CDR publishes nacked by the broker since the last such line, each dropped; \
                 said at most once per interval"
            );
        }
        self.dropped(handed_at, CdrDrop::Nacked);
    }

    /// Tells the observer, which returns or panics: a panic is caught here,
    /// said at most once per [`LINE_INTERVAL`], and the end counts all the
    /// same.
    fn tell(&self, delivery: CdrDelivery, handed_at: Instant) {
        let Some(observer) = &self.observer else {
            return;
        };
        let told = std::panic::catch_unwind(AssertUnwindSafe(|| {
            observer.ended(delivery, handed_at.elapsed());
        }));
        if told.is_err() {
            if let Some(panics) = self.observer_panics.counted() {
                tracing::error!(
                    panics,
                    interval_ms = millis_u64(LINE_INTERVAL),
                    "the CDR delivery observer panicked since the last such line; each record \
                     counts all the same; said at most once per interval"
                );
            }
        }
    }
}
