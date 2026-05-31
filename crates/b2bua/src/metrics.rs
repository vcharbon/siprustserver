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

    /// Render the counter set as Prometheus text-exposition format. Used by the
    /// runner's `/metrics` endpoint so an endurance recorder can scrape worker
    /// application metrics alongside container CPU/memory. The
    /// creations/removals pair also yields a live `active_calls` gauge.
    pub fn prometheus_text(&self) -> String {
        let creations = self.creations_total();
        let removals = self.removals_total();
        let active = creations.saturating_sub(removals);
        let mut s = String::with_capacity(2048);
        let mut counter = |name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
        };
        counter("b2bua_dispatch_queue_drops_total", "events dropped: per-call queue full", self.queue_drops_total());
        counter("b2bua_dispatch_cap_drops_total", "events dropped: global call cap reached", self.cap_drops_total());
        counter("b2bua_dispatch_saturation_total", "global handler concurrency saturation hits", self.saturation_total());
        counter("b2bua_call_creations_total", "per-call dispatch queues created", creations);
        counter("b2bua_call_removals_total", "per-call dispatch queues removed", removals);
        counter("b2bua_handler_timeouts_total", "handler executions that timed out", self.handler_timeouts_total());
        counter("b2bua_force_purge_total", "calls force-purged (loop guard)", self.force_purge_total());
        counter("b2bua_fast_reject_terminating_total", "requests fast-rejected on a terminating call", self.fast_reject_terminating_total());
        counter("b2bua_unroutable_dropped_total", "messages dropped: no route resolved", self.unroutable_dropped_total());
        counter("b2bua_cdr_written_total", "CDRs written", self.cdr_written_total());
        counter("b2bua_cdr_dropped_total", "CDRs dropped on buffer overflow", self.cdr_dropped_total());
        s.push_str("# HELP b2bua_active_calls live calls (creations - removals)\n# TYPE b2bua_active_calls gauge\n");
        s.push_str(&format!("b2bua_active_calls {active}\n"));
        s
    }
}
