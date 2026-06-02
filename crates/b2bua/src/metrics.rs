//! B2BUA metrics — atomic counters/gauges (the source's `MetricsRegistry`
//! surface reduced to the counters the ported paths move). Cheap to clone
//! (one `Arc`); read with the `*_total` accessors.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Inner {
    // per-method request + per-(method,code) response counters (data-path
    // visibility: which SIP methods/response codes the worker is moving).
    requests: Mutex<BTreeMap<String, u64>>,   // keyed method
    responses: Mutex<BTreeMap<String, u64>>,  // keyed "cseq_method|status_code"
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
    // replication (peer-to-peer HA; separate namespace `b2bua_repl_*`). These
    // localise an HA failure to a layer: `flush_propagated` rising on the PRIMARY
    // proves it is attempting to replicate (the proxy cookie stamped
    // `topology.bak`); `pull_applied` rising + `backup_held` > 0 on the BACKUP
    // proves the replica actually arrived; `takeover_resolved`/`hydrated` prove a
    // failed-over in-dialog request found + loaded the replica on the backup.
    repl_flush_propagated: AtomicU64,
    repl_pull_applied: AtomicU64,
    repl_backup_held: AtomicU64, // gauge: replicas currently held as backup
    repl_takeover_resolved: AtomicU64,
    repl_takeover_hydrated: AtomicU64,
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

    /// Count one inbound request by SIP method, for `b2bua_requests_total{method}`.
    pub fn record_request(&self, method: &str) {
        *self.inner.requests.lock().unwrap().entry(method.to_ascii_uppercase()).or_insert(0) += 1;
    }

    /// Count one inbound response by its CSeq method + status code, for
    /// `b2bua_responses_total{method,code}`.
    pub fn record_response(&self, method: &str, code: u16) {
        let key = format!("{}|{}", method.to_ascii_uppercase(), code);
        *self.inner.responses.lock().unwrap().entry(key).or_insert(0) += 1;
    }

    // --- replication ---
    counter!(bump_repl_flush_propagated, repl_flush_propagated_total, repl_flush_propagated);
    counter!(bump_repl_pull_applied, repl_pull_applied_total, repl_pull_applied);
    counter!(bump_repl_takeover_resolved, repl_takeover_resolved_total, repl_takeover_resolved);
    counter!(bump_repl_takeover_hydrated, repl_takeover_hydrated_total, repl_takeover_hydrated);

    /// A backup replica was admitted to a backup partition (puller applied a
    /// `Create`). Pairs with [`dec_repl_backup_held`](Self::dec_repl_backup_held)
    /// to track the live replica count this node holds for its peers.
    pub fn inc_repl_backup_held(&self) {
        self.inner.repl_backup_held.fetch_add(1, Ordering::Relaxed);
    }
    /// A backup replica left a backup partition (puller applied a `Delete`).
    pub fn dec_repl_backup_held(&self) {
        // Saturating: a Delete with no prior Create (cold) must not underflow.
        let _ = self
            .inner
            .repl_backup_held
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_sub(1)));
    }
    pub fn repl_backup_held(&self) -> u64 {
        self.inner.repl_backup_held.load(Ordering::Relaxed)
    }

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
        counter("b2bua_call_creations_total", "B2BUA calls this worker began serving (one per call_ref / dialog; NOT transactions or SIP messages). Matched 1:1 with removals.", creations);
        counter("b2bua_call_removals_total", "B2BUA calls this worker stopped serving (one per call_ref teardown). Matched 1:1 with creations.", removals);
        counter("b2bua_handler_timeouts_total", "handler executions that timed out", self.handler_timeouts_total());
        counter("b2bua_force_purge_total", "calls force-purged (loop guard)", self.force_purge_total());
        counter("b2bua_fast_reject_terminating_total", "requests fast-rejected on a terminating call", self.fast_reject_terminating_total());
        counter("b2bua_unroutable_dropped_total", "messages dropped: no route resolved", self.unroutable_dropped_total());
        counter("b2bua_cdr_written_total", "CDRs written", self.cdr_written_total());
        counter("b2bua_cdr_dropped_total", "CDRs dropped on buffer overflow", self.cdr_dropped_total());
        // ── replication (peer-to-peer HA) — own namespace, distinct from the
        // data-path counters above so an HA failure can be localised by layer. ──
        counter("b2bua_repl_flush_propagated_total", "primary flushes that propagated to a backup peer (topology.bak set)", self.repl_flush_propagated_total());
        counter("b2bua_repl_pull_applied_total", "inbound replica entries applied from a peer's changelog", self.repl_pull_applied_total());
        counter("b2bua_repl_takeover_resolved_total", "in-dialog requests whose callRef was recovered from the replica index (acting-backup)", self.repl_takeover_resolved_total());
        counter("b2bua_repl_takeover_hydrated_total", "calls hydrated from a backup replica to serve a failed-over request", self.repl_takeover_hydrated_total());

        // Per-method request + per-(method,code) response counters. Drop the
        // `counter` closure's borrow first by ending the block above.
        s.push_str("# HELP b2bua_requests_total inbound SIP requests by method\n# TYPE b2bua_requests_total counter\n");
        for (method, v) in self.inner.requests.lock().unwrap().iter() {
            s.push_str(&format!("b2bua_requests_total{{method=\"{method}\"}} {v}\n"));
        }
        s.push_str("# HELP b2bua_responses_total inbound SIP responses by CSeq method + status code\n# TYPE b2bua_responses_total counter\n");
        for (k, v) in self.inner.responses.lock().unwrap().iter() {
            let (method, code) = k.split_once('|').unwrap_or((k.as_str(), ""));
            s.push_str(&format!("b2bua_responses_total{{method=\"{method}\",code=\"{code}\"}} {v}\n"));
        }

        // Gauges last (direct writes — they end the `counter` closure's borrow).
        s.push_str("# HELP b2bua_active_calls live calls this worker is serving (creations - removals; now a true gauge since the two are paired)\n# TYPE b2bua_active_calls gauge\n");
        s.push_str(&format!("b2bua_active_calls {active}\n"));
        s.push_str("# HELP b2bua_repl_backup_held replicas currently held in backup partitions for peers\n# TYPE b2bua_repl_backup_held gauge\n");
        s.push_str(&format!("b2bua_repl_backup_held {}\n", self.repl_backup_held()));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_method_request_response_render() {
        let m = B2buaMetrics::new();
        m.record_request("invite");
        m.record_request("BYE");
        m.record_response("invite", 200);
        m.record_response("BYE", 200);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_requests_total{method=\"INVITE\"} 1"));
        assert!(txt.contains("b2bua_requests_total{method=\"BYE\"} 1"));
        assert!(txt.contains("b2bua_responses_total{method=\"INVITE\",code=\"200\"} 1"));
        assert!(txt.contains("b2bua_responses_total{method=\"BYE\",code=\"200\"} 1"));
    }
}
