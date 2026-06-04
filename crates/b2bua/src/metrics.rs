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
    // timer service (gauges): physical DelayQueue size (live entries + not-yet-
    // expired tombstones from cancelled/rescheduled timers) vs. the live
    // schedulable timer count. `queue_len - live` is the lingering-tombstone
    // backlog — the work that grows with cancelled long-interval timers
    // (e.g. the per-call 1 h GlobalDuration) even while active_calls is flat.
    timer_queue_len: AtomicU64,
    timer_live: AtomicU64,
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
    // X11 fail-back: `reclaimed` = calls a rebooted primary re-materialised into
    // its live map (active reclaim); `handback` = ghost-backup takeover copies a
    // backup deactivated on a primary's `Deactivate`. After a kill_worker+reclaim,
    // `handback` ≈ duplicates released and the active/sipp gap reaps to ~0.
    repl_reclaimed: AtomicU64,
    repl_handback: AtomicU64,
    // X11 EAGER takeover: a survivor materialising a dead peer's `bak:` partition
    // into its live map on the peer-`Removed` membership delta — the death-driven
    // analogue of `takeover_hydrated` (which is request-driven). This is what keeps
    // a QUIESCENT long-hold dialog alive after its primary is killed: nothing
    // inbound ever arrives to trigger the lazy hydrate, so the survivor must reclaim
    // it eagerly. Pairs with `handback` (the rebooted primary later reclaims + the
    // survivor hands its eager copy back → exactly one owner).
    repl_eager_takeover: AtomicU64,
    // Memory-attribution gauges (sampled, not counter-derived). `store_calls` is
    // the TRUE live call-map length — compare to `active_calls`
    // (creations-removals); a divergence localises a store-side leak the counter
    // pair can't see. The sibling maps should track `store_calls`; one that grows
    // while it stays flat names the leaking map (`locks` + `takeover_at` are the
    // X11 fail-back suspects: a per-call lock or takeover-instant never released).
    store_calls: AtomicU64,
    store_sip_index: AtomicU64,
    store_indexed: AtomicU64,
    store_locks: AtomicU64,
    store_takeover_at: AtomicU64,
    // Replicating-store sizes: `repl_meta_total` = all replica metadata entries
    // this node holds; `repl_meta_backup` = the BACKUP-partition subset (the
    // ghost-backup takeover copies the X11 Deactivate handback must release — if
    // this climbs unbounded after failovers, handback isn't reaping). The
    // changelog gauges are the outbound replication buffer depth (entries across
    // peers + live peer count); a peer whose entries grow without draining
    // (slow/dead subscriber) is an outbound-side leak distinct from the call map.
    repl_meta_total: AtomicU64,
    repl_meta_backup: AtomicU64,
    repl_changelog_entries: AtomicU64,
    repl_changelog_peers: AtomicU64,
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
    counter!(bump_repl_reclaimed, repl_reclaimed_total, repl_reclaimed);
    counter!(bump_repl_handback, repl_handback_total, repl_handback);
    counter!(bump_repl_eager_takeover, repl_eager_takeover_total, repl_eager_takeover);

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

    /// Set the timer-service gauges from the driver on each state change.
    /// `queue_len` is the physical `DelayQueue` size (live entries + not-yet-
    /// expired tombstones); `live` is the number of schedulable timers. Their
    /// difference is the lingering-tombstone backlog (see field docs).
    pub fn set_timer_gauges(&self, queue_len: u64, live: u64) {
        self.inner.timer_queue_len.store(queue_len, Ordering::Relaxed);
        self.inner.timer_live.store(live, Ordering::Relaxed);
    }
    pub fn timer_queue_len(&self) -> u64 {
        self.inner.timer_queue_len.load(Ordering::Relaxed)
    }
    pub fn timer_live(&self) -> u64 {
        self.inner.timer_live.load(Ordering::Relaxed)
    }

    /// Push the call-store map lengths (memory-attribution gauges). Sampled
    /// periodically by the runner under the store's own lock. `calls` is the
    /// true live call-map size; the rest are its sibling indexes + per-call
    /// state. See the field docs for what a divergence localises.
    pub fn set_store_gauges(
        &self,
        calls: u64,
        sip_index: u64,
        indexed: u64,
        locks: u64,
        takeover_at: u64,
    ) {
        self.inner.store_calls.store(calls, Ordering::Relaxed);
        self.inner.store_sip_index.store(sip_index, Ordering::Relaxed);
        self.inner.store_indexed.store(indexed, Ordering::Relaxed);
        self.inner.store_locks.store(locks, Ordering::Relaxed);
        self.inner.store_takeover_at.store(takeover_at, Ordering::Relaxed);
    }

    /// Push the replicating-store sizes (memory-attribution gauges): total +
    /// backup-partition replica metadata entries, and the outbound changelog
    /// depth (entries across peers + peer count). See the field docs.
    pub fn set_repl_store_gauges(
        &self,
        meta_total: u64,
        meta_backup: u64,
        changelog_entries: u64,
        changelog_peers: u64,
    ) {
        self.inner.repl_meta_total.store(meta_total, Ordering::Relaxed);
        self.inner.repl_meta_backup.store(meta_backup, Ordering::Relaxed);
        self.inner.repl_changelog_entries.store(changelog_entries, Ordering::Relaxed);
        self.inner.repl_changelog_peers.store(changelog_peers, Ordering::Relaxed);
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
        counter("b2bua_repl_reclaimed_total", "calls a rebooted primary re-materialised into its live map + re-armed (active reclaim, ADR-0011 X11)", self.repl_reclaimed_total());
        counter("b2bua_repl_handback_total", "ghost-backup takeover copies deactivated on a primary's Deactivate handback (ADR-0011 X11)", self.repl_handback_total());
        counter("b2bua_repl_eager_takeover_total", "calls a survivor eagerly materialised from a dead peer's backup partition on a peer-Removed delta (X11 — keeps quiescent dialogs alive)", self.repl_eager_takeover_total());

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
        // Timer-queue gauges: physical DelayQueue size vs. live timers. A
        // queue_len that climbs while timer_live (and active_calls) stay flat is
        // the lingering-tombstone backlog of cancelled long-interval timers — the
        // CPU drift that looks like a leak but isn't one.
        s.push_str("# HELP b2bua_timer_queue_len physical timer DelayQueue entries, incl. not-yet-expired tombstones from cancelled/rescheduled timers\n# TYPE b2bua_timer_queue_len gauge\n");
        s.push_str(&format!("b2bua_timer_queue_len {}\n", self.timer_queue_len()));
        s.push_str("# HELP b2bua_timer_live live (schedulable) timers; b2bua_timer_queue_len minus this is the lingering-tombstone backlog\n# TYPE b2bua_timer_live gauge\n");
        s.push_str(&format!("b2bua_timer_live {}\n", self.timer_live()));
        // Memory-attribution gauges: per-map sizes so a RSS climb can be pinned
        // to a specific map even when active_calls is flat. b2bua_store_calls is
        // the TRUE live call-map length — a gap vs b2bua_active_calls localises a
        // store-side leak; a sibling map (sip_index/indexed/locks/takeover_at)
        // outgrowing it names which one.
        let g = |s: &mut String, name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
        };
        g(&mut s, "b2bua_store_calls", "live entries in the call map (true gauge; compare to b2bua_active_calls)", self.inner.store_calls.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_sip_index", "SIP routing index keys (callId/tag -> callRef)", self.inner.store_sip_index.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_indexed", "per-call owned-index-key sets", self.inner.store_indexed.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_locks", "per-callRef serialization locks held (should track store_calls; a gap is a lock leak)", self.inner.store_locks.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_takeover_at", "per-call takeover activation instants (X11; should track backup copies, reaped on handback)", self.inner.store_takeover_at.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_total", "replica metadata entries held (all partitions)", self.inner.repl_meta_total.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_backup", "replica metadata entries in BACKUP partitions (ghost-backup copies the X11 handback must release)", self.inner.repl_meta_backup.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_entries", "outbound changelog entries across all peer logs (replication buffer depth)", self.inner.repl_changelog_entries.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_peers", "peer logs currently held in the changelog", self.inner.repl_changelog_peers.load(Ordering::Relaxed));
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

    #[test]
    fn memory_attribution_gauges_render() {
        let m = B2buaMetrics::new();
        // Unset → render at 0 (a flat gauge, not a missing series).
        let zero = m.prometheus_text();
        assert!(zero.contains("b2bua_store_calls 0"));
        assert!(zero.contains("b2bua_repl_meta_backup 0"));

        m.set_store_gauges(7, 11, 7, 9, 3);
        m.set_repl_store_gauges(40, 22, 64, 4);
        let txt = m.prometheus_text();
        // A store_locks (9) > store_calls (7) gap is exactly the lock-leak signal.
        assert!(txt.contains("b2bua_store_calls 7"));
        assert!(txt.contains("b2bua_store_sip_index 11"));
        assert!(txt.contains("b2bua_store_indexed 7"));
        assert!(txt.contains("b2bua_store_locks 9"));
        assert!(txt.contains("b2bua_store_takeover_at 3"));
        assert!(txt.contains("b2bua_repl_meta_total 40"));
        assert!(txt.contains("b2bua_repl_meta_backup 22"));
        assert!(txt.contains("b2bua_repl_changelog_entries 64"));
        assert!(txt.contains("b2bua_repl_changelog_peers 4"));
        // Each gauge series must carry its TYPE line (Prometheus exposition).
        assert!(txt.contains("# TYPE b2bua_store_calls gauge"));
        assert!(txt.contains("# TYPE b2bua_repl_meta_backup gauge"));
    }
}
