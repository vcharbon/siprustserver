//! B2BUA metrics — atomic counters/gauges (the source's `MetricsRegistry`
//! surface reduced to the counters the ported paths move). Cheap to clone
//! (one `Arc`); read with the `*_total` accessors.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Default)]
struct Inner {
    // dispatcher
    queue_drops: AtomicU64,
    cap_drops: AtomicU64,
    saturation: AtomicU64,
    creations: AtomicU64,
    removals: AtomicU64,
    // router / handler
    handler_timeouts: AtomicU64,
    force_purge: AtomicU64,
    fast_reject_terminating: AtomicU64,
    unroutable_dropped: AtomicU64,
    // cdr
    cdr_written: AtomicU64,
    cdr_dropped: AtomicU64,
}

/// Clone-cheap handle to the B2BUA counter set.
#[derive(Debug, Clone, Default)]
pub struct B2buaMetrics {
    inner: Arc<Inner>,
}

macro_rules! counter {
    ($bump:ident, $get:ident, $field:ident) => {
        pub fn $bump(&self) {
            self.inner.$field.fetch_add(1, Ordering::Relaxed);
        }
        pub fn $get(&self) -> u64 {
            self.inner.$field.load(Ordering::Relaxed)
        }
    };
}

impl B2buaMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    counter!(bump_queue_drop, queue_drops_total, queue_drops);
    counter!(bump_cap_drop, cap_drops_total, cap_drops);
    counter!(bump_saturation, saturation_total, saturation);
    counter!(bump_creation, creations_total, creations);
    counter!(bump_removal, removals_total, removals);
    counter!(bump_handler_timeout, handler_timeouts_total, handler_timeouts);
    counter!(bump_force_purge, force_purge_total, force_purge);
    counter!(
        bump_fast_reject_terminating,
        fast_reject_terminating_total,
        fast_reject_terminating
    );
    counter!(bump_unroutable_dropped, unroutable_dropped_total, unroutable_dropped);
    counter!(bump_cdr_written, cdr_written_total, cdr_written);
    counter!(bump_cdr_dropped, cdr_dropped_total, cdr_dropped);
}
