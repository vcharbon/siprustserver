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
    requests: Mutex<BTreeMap<String, u64>>,   // keyed method (INBOUND)
    requests_out: Mutex<BTreeMap<String, u64>>, // keyed method (OUTBOUND — originated/relayed)
    responses: Mutex<BTreeMap<String, u64>>,  // keyed "cseq_method|status_code" (INBOUND)
    // Replication serve-side liveness: per `(flow, peer)` count of catch-up/idle
    // `Noop`s this node SENT as a server (keyed "flow|peer"). A `Noop` means "I am
    // caught up — I have sent you everything in this flow's keyspace" (ADR-0014
    // §Stream topology). It MUST climb continuously (the ~20s idle floor) on every
    // healthy stream — from the backup-holder's point of view, proof it has flushed
    // all the peer's reclaimable/backed-up calls. A flatlined series names a stuck
    // serve loop / dead subscriber the body-count gauges can't see.
    repl_noops_sent: Mutex<BTreeMap<String, u64>>,
    // dispatcher
    queue_drops: AtomicU64,
    cap_drops: AtomicU64,
    saturation: AtomicU64,
    // MAX_MESSAGES_PER_CALL cap-defense: calls torn down for crossing the
    // per-call message cap (a runaway re-INVITE/OPTIONS storm or glare loop).
    // Port of the TS SipRouter cap (was missing in the Rust port — a call could
    // process unbounded in-dialog events, ratcheting txn/clone/store churn).
    message_cap_terminated: AtomicU64,
    creations: AtomicU64,
    removals: AtomicU64,
    // router / handler
    handler_timeouts: AtomicU64,
    force_purge: AtomicU64,
    fast_reject_terminating: AtomicU64,
    unroutable_dropped: AtomicU64,
    // call reaper (ADR-0020). `handler_panics` counts dispatcher-observed body
    // panics (pre-reaper these were swallowed — the zero-CDR leak class);
    // `reaper_verdicts` counts injected synthetic events (stale + fatal +
    // discharge); `reaper_discharged` is the ALARM — the rules path itself
    // failed twice for a call; expected ~0 in any healthy run.
    handler_panics: AtomicU64,
    reaper_sweeps: AtomicU64,
    reaper_verdicts: AtomicU64,
    reaper_discharged: AtomicU64,
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
    // `topology.bak`); the per-stream `applied` breakdown (below) proves the
    // replica actually arrived; `takeover_resolved`/`hydrated` prove a failed-over
    // in-dialog request found + loaded the replica on the backup. The TRUE resident
    // backup count is the sampled `repl_meta_backup` gauge (not a counter-derived
    // estimate). The old `repl_pull_applied` aggregate + the `repl_backup_held`
    // gauge were removed: the former is superseded by the labelled `applied`
    // breakdown, the latter double-counted (inc/dec only on apply, never on TTL
    // eviction) and is replaced by `repl_meta_backup`.
    repl_flush_propagated: AtomicU64,
    // Inbound replication ops applied, per `(flow, peer, op)` (keyed
    // "flow|peer|op"): `flow` = recovery (Pri/reclaim — our own calls pulled back
    // from a peer's backup) | backup (Bak — a peer's calls we hold as backup);
    // `peer` = the endpoint streamed from; `op` = create | update | delete. This is
    // the REAL per-stream replication signal: a reboot's bulk reclaim shows as a
    // sharp step in `recovery`/`create` for that peer (the "huge fast bump" the
    // aggregate counter hid).
    repl_applied: Mutex<BTreeMap<String, u64>>,
    repl_takeover_resolved: AtomicU64,
    repl_takeover_hydrated: AtomicU64,
    // Fail-back (ADR-0011 X11 / ADR-0014): `reclaimed` = calls a rebooted primary
    // re-materialised into its live map (active reclaim); `self_release` = acting-
    // backup takeover copies the backup *self-released* once the transaction(s) it
    // served reached a terminal state (ADR-0014 — replaces the `Deactivate`
    // handback). After a kill_worker+reclaim, `self_release` ≈ takeover copies shed
    // and the active/sipp gap reaps to ~0.
    repl_reclaimed: AtomicU64,
    repl_self_release: AtomicU64,
    // Model Y (ADR-0020 X3): a backup-held deferred terminal whose primary never
    // came back to reclaim it (crashed for good, past the replica TTL). The backup
    // is NOT a discharge authority, so its periodic reap releases the call's limiter
    // hold(s) + frees the replica memory but writes **no CDR** — the CDR accounting
    // is the accepted loss of the double-failure (primary down AND never returns).
    // This counter is that lost-CDR count: it should stay ~0 in a healthy cluster
    // and only climbs when a primary is permanently lost mid-call.
    repl_terminal_lost: AtomicU64,
    // Re-hydration diagnostics (long-call-on-reboot study, 2026-06-05). How a
    // rebooted primary's bootstrap passes terminate: `seeded` = a pass reached
    // the first catch-up `Noop` (the peer streamed the full `bak:{me}` keyset);
    // `stalled` = a pass hit the bootstrap hard deadline before that Noop arrived
    // (marked complete best-effort, partial pre-seed materialised, then KEEPS
    // streaming on the same socket — not a disconnect). `last_applied` (gauge) =
    // bodies the MOST RECENT pass imported. The decisive signal: if `stalled`
    // climbs and `last_applied` keeps re-stalling at the SAME value across passes,
    // the STREAM is truncating (a peer-side stall), not just the materialisation —
    // and a longer hard deadline alone would not help. If `seeded` bumps and
    // `repl_reclaimed_total` ≈ the held count, re-hydration is whole.
    repl_bootstrap_seeded: AtomicU64,
    repl_bootstrap_stalled: AtomicU64,
    repl_bootstrap_last_applied: AtomicU64,
    // Reboot-reclaim completeness (long-call-on-reboot study, 2026-06-06). Per the
    // MOST RECENT bulk reclaim pass (`router::reclaim_all`): `scanned` = bodies
    // found in `pri:{self}` (the denominator — everything the bootstrap import made
    // reclaimable on this node) and `materialized` = how many of those this pass
    // freshly inserted into the live serving map + re-armed timers. The per-reboot
    // chain localises exactly where a rebooted primary's quiescent dialogs are
    // lost: `(peer) repl_meta_backup` → `repl_bootstrap_last_applied` →
    // `repl_reclaim_scanned` → `repl_reclaim_materialized`. `scanned ≪ peer
    // meta_backup` ⇒ a bootstrap-import / forward-replication gap; `materialized ≪
    // scanned` (cumulatively, via `repl_reclaimed_total`) ⇒ a materialise gap.
    repl_reclaim_scanned: AtomicU64,
    repl_reclaim_materialized: AtomicU64,
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
    store_touched: AtomicU64,
    // Replicating-store sizes: `repl_meta_total` = all replica metadata entries
    // this node holds; `repl_meta_backup` = the BACKUP-partition subset (the
    // replicas this node holds for its peers; a backup self-releases its *live*
    // takeover copy on transaction completion but KEEPS the replica until its
    // primary deletes it, so this tracks resident backup bodies, ADR-0014). The
    // changelog gauges are the outbound replication buffer depth (entries across
    // peers + live peer count); a peer whose entries grow without draining
    // (slow/dead subscriber) is an outbound-side leak distinct from the call map.
    repl_meta_total: AtomicU64,
    repl_meta_backup: AtomicU64,
    repl_changelog_entries: AtomicU64,
    repl_changelog_peers: AtomicU64,
    // State-machine cursor census (ADR-0016 slice 9), keyed "machine|state": the
    // number of LIVE calls resting at each machine cursor, sampled from the call
    // map alongside the store gauges (not on the hot path). Renders as
    // `b2bua_sm_cursors{machine,state}` — the live distribution of every call's
    // machine positions (`global-call` always; `transfer`/`announcement` while a
    // service is active). A service that won't drain (stuck announcement, a
    // backup-partition dialog never reconciled) shows here as a cursor census
    // that lingers while `active_calls` is otherwise quiet.
    sm_cursors: Mutex<BTreeMap<String, u64>>,
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

    counter!(bump_message_cap_terminated, message_cap_terminated_total, message_cap_terminated);
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
    // --- call reaper (ADR-0020) ---
    counter!(bump_handler_panic, handler_panics_total, handler_panics);
    counter!(bump_reaper_sweep, reaper_sweeps_total, reaper_sweeps);
    counter!(bump_reaper_verdict, reaper_verdicts_total, reaper_verdicts);
    counter!(bump_reaper_discharged, reaper_discharged_total, reaper_discharged);

    /// Count one catch-up/idle `Noop` SENT on a serve-side stream, for
    /// `b2bua_repl_noops_sent_total{flow,peer}` (ADR-0014). `flow` is the stream
    /// kind (`reclaim` = `Pri`, `backup` = `Bak`); `peer` is the pulling caller.
    /// Climbs continuously on a healthy stream (the ~20s idle floor) — the
    /// backup-holder's "I have sent you everything in this flow" liveness sign.
    pub fn record_repl_noop_sent(&self, flow: &str, peer: &str) {
        *self
            .inner
            .repl_noops_sent
            .lock()
            .unwrap()
            .entry(format!("{flow}|{peer}"))
            .or_insert(0) += 1;
    }

    /// Count one inbound request by SIP method, for `b2bua_requests_total{method}`.
    pub fn record_request(&self, method: &str) {
        *self.inner.requests.lock().unwrap().entry(method.to_ascii_uppercase()).or_insert(0) += 1;
    }

    /// Count one OUTBOUND request this worker originated/relayed, for
    /// `b2bua_requests_out_total{method}`. The in-dialog keepalive OPTIONS lands
    /// here; pairing OPTIONS-out with the inbound `responses_total{OPTIONS,200}`
    /// isolates the keepalive round-trip (sent vs answered) on the b2bua itself.
    pub fn record_request_out(&self, method: &str) {
        *self.inner.requests_out.lock().unwrap().entry(method.to_ascii_uppercase()).or_insert(0) += 1;
    }

    /// Count one inbound response by its CSeq method + status code, for
    /// `b2bua_responses_total{method,code}`.
    pub fn record_response(&self, method: &str, code: u16) {
        let key = format!("{}|{}", method.to_ascii_uppercase(), code);
        *self.inner.responses.lock().unwrap().entry(key).or_insert(0) += 1;
    }

    // --- replication ---
    counter!(bump_repl_flush_propagated, repl_flush_propagated_total, repl_flush_propagated);
    counter!(bump_repl_takeover_resolved, repl_takeover_resolved_total, repl_takeover_resolved);
    counter!(bump_repl_takeover_hydrated, repl_takeover_hydrated_total, repl_takeover_hydrated);
    counter!(bump_repl_reclaimed, repl_reclaimed_total, repl_reclaimed);
    counter!(bump_repl_self_release, repl_self_release_total, repl_self_release);
    counter!(bump_repl_terminal_lost, repl_terminal_lost_total, repl_terminal_lost);
    counter!(bump_repl_bootstrap_seeded, repl_bootstrap_seeded_total, repl_bootstrap_seeded);
    counter!(bump_repl_bootstrap_stalled, repl_bootstrap_stalled_total, repl_bootstrap_stalled);

    /// Record how many bodies the most recent bootstrap pass imported (gauge).
    pub fn set_repl_bootstrap_last_applied(&self, n: u64) {
        self.inner.repl_bootstrap_last_applied.store(n, Ordering::Relaxed);
    }
    pub fn repl_bootstrap_last_applied(&self) -> u64 {
        self.inner.repl_bootstrap_last_applied.load(Ordering::Relaxed)
    }

    /// Record the most recent bulk-reclaim pass's `(scanned, materialized)` — the
    /// reboot-reclaim completeness denominator/numerator (gauges). `scanned` is the
    /// `pri:{self}` partition size the pass swept; `materialized` is how many it
    /// freshly re-served. Overwritten each pass; the cumulative materialised total
    /// is `repl_reclaimed_total`.
    pub fn set_repl_reclaim_pass(&self, scanned: u64, materialized: u64) {
        self.inner.repl_reclaim_scanned.store(scanned, Ordering::Relaxed);
        self.inner.repl_reclaim_materialized.store(materialized, Ordering::Relaxed);
    }
    pub fn repl_reclaim_scanned(&self) -> u64 {
        self.inner.repl_reclaim_scanned.load(Ordering::Relaxed)
    }
    pub fn repl_reclaim_materialized(&self) -> u64 {
        self.inner.repl_reclaim_materialized.load(Ordering::Relaxed)
    }

    /// Record one inbound replication op applied, for
    /// `b2bua_repl_applied_total{flow,peer,op}`. `flow` = `recovery` (Pri/reclaim)
    /// | `backup` (Bak); `op` = `create` | `update` | `delete`. The real per-stream
    /// replication signal (a reboot's bulk reclaim shows as a `recovery`/`create`
    /// step for that peer).
    pub fn record_repl_applied(&self, flow: &str, peer: &str, op: &str) {
        *self
            .inner
            .repl_applied
            .lock()
            .unwrap()
            .entry(format!("{flow}|{peer}|{op}"))
            .or_insert(0) += 1;
    }
    /// Sum of all applied replication ops (test/observability convenience —
    /// replaces the retired `repl_pull_applied_total` aggregate).
    pub fn repl_applied_sum(&self) -> u64 {
        self.inner.repl_applied.lock().unwrap().values().sum()
    }
    /// Backup replicas this node currently holds, derived from the `backup`-flow
    /// op counts (creates − deletes). Test/observability convenience replacing the
    /// retired `repl_backup_held` gauge; production reads the accurate sampled
    /// `repl_meta_backup` (this derivation, like the old gauge, does not see TTL
    /// eviction — fine for the unit tests that never evict).
    pub fn repl_backup_replicas(&self) -> u64 {
        let m = self.inner.repl_applied.lock().unwrap();
        let get = |op: &str| {
            m.iter()
                .filter(|(k, _)| {
                    let mut p = k.split('|');
                    p.next() == Some("backup") && p.nth(1) == Some(op)
                })
                .map(|(_, v)| *v)
                .sum::<u64>()
        };
        get("create").saturating_sub(get("delete"))
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
        touched: u64,
    ) {
        self.inner.store_calls.store(calls, Ordering::Relaxed);
        self.inner.store_sip_index.store(sip_index, Ordering::Relaxed);
        self.inner.store_indexed.store(indexed, Ordering::Relaxed);
        self.inner.store_locks.store(locks, Ordering::Relaxed);
        self.inner.store_takeover_at.store(takeover_at, Ordering::Relaxed);
        self.inner.store_touched.store(touched, Ordering::Relaxed);
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

    /// Replace the state-machine cursor census (ADR-0016 slice 9) wholesale —
    /// `census` maps `(machine, state)` to the count of live calls resting there,
    /// sampled from the call map under the store lock on the slow gauge cadence.
    /// Overwriting (rather than incrementing) means a cursor that drained to zero
    /// disappears from the next scrape instead of sticking at its last value.
    pub fn set_sm_cursor_census(&self, census: BTreeMap<(String, String), u64>) {
        let mut map = self.inner.sm_cursors.lock().unwrap();
        map.clear();
        for ((machine, state), n) in census {
            map.insert(format!("{machine}|{state}"), n);
        }
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
        counter("b2bua_message_cap_terminated_total", "calls terminated for exceeding max_messages_per_call (cap-defense; a climbing rate names a runaway-traffic call class)", self.message_cap_terminated_total());
        counter("b2bua_dispatch_queue_drops_total", "events dropped: per-call queue full", self.queue_drops_total());
        counter("b2bua_dispatch_cap_drops_total", "events dropped: global call cap reached", self.cap_drops_total());
        counter("b2bua_dispatch_saturation_total", "global handler concurrency saturation hits", self.saturation_total());
        counter("b2bua_call_creations_total", "B2BUA calls this worker began serving (one per call_ref / dialog; NOT transactions or SIP messages). Matched 1:1 with removals.", creations);
        counter("b2bua_call_removals_total", "B2BUA calls this worker stopped serving (one per call_ref teardown). Matched 1:1 with creations.", removals);
        counter("b2bua_handler_timeouts_total", "handler executions that timed out", self.handler_timeouts_total());
        counter("b2bua_force_purge_total", "calls force-purged (loop guard)", self.force_purge_total());
        counter("b2bua_fast_reject_terminating_total", "requests fast-rejected on a terminating call", self.fast_reject_terminating_total());
        counter("b2bua_unroutable_dropped_total", "messages dropped: no route resolved", self.unroutable_dropped_total());
        counter("b2bua_cdr_written_total", "CDRs successfully written to the sink", self.cdr_written_total());
        counter("b2bua_cdr_dropped_total", "CDRs dropped (submit-queue overflow or sink failure)", self.cdr_dropped_total());
        // ── call reaper (ADR-0020) ──
        counter("b2bua_handler_panics_total", "handler bodies that panicked (dispatcher-observed; each becomes a reaper strike instead of a silent call leak)", self.handler_panics_total());
        counter("b2bua_reaper_sweeps_total", "reaper sweep ticks executed", self.reaper_sweeps_total());
        counter("b2bua_reaper_verdicts_total", "reaper verdicts injected (stale + fatal-error + discharge synthetic events)", self.reaper_verdicts_total());
        counter("b2bua_reaper_discharged_total", "strike-2 discharges: the rules path itself failed for a call and the snapshot was forced terminal directly (ALARM: expected ~0)", self.reaper_discharged_total());
        // ── replication (peer-to-peer HA) — own namespace, distinct from the
        // data-path counters above so an HA failure can be localised by layer. ──
        counter("b2bua_repl_flush_propagated_total", "primary flushes that propagated to a backup peer (topology.bak set)", self.repl_flush_propagated_total());
        counter("b2bua_repl_takeover_resolved_total", "in-dialog requests whose callRef was recovered from the replica index (acting-backup)", self.repl_takeover_resolved_total());
        counter("b2bua_repl_takeover_hydrated_total", "calls hydrated from a backup replica to serve a failed-over request", self.repl_takeover_hydrated_total());
        counter("b2bua_repl_reclaimed_total", "calls a rebooted primary re-materialised into its live map + re-armed (active reclaim, ADR-0011 X11)", self.repl_reclaimed_total());
        counter("b2bua_repl_self_release_total", "acting-backup takeover copies self-released once their served transaction(s) reached a terminal state (ADR-0014, replaces the Deactivate handback)", self.repl_self_release_total());
        counter("b2bua_repl_terminal_lost_total", "backup-held deferred terminals whose primary never reclaimed them (dead past the replica TTL): limiter released + memory freed by the periodic reap, but NO CDR — the accepted lost-CDR double-failure (ADR-0020 X3)", self.repl_terminal_lost_total());
        counter("b2bua_repl_bootstrap_seeded_total", "rebooted-primary bootstrap passes that reached the first catch-up Noop (peer streamed the full bak:{me} keyset)", self.repl_bootstrap_seeded_total());
        counter("b2bua_repl_bootstrap_stalled_total", "rebooted-primary bootstrap passes that hit the bootstrap hard deadline before the first Noop (best-effort completion; keeps streaming on the same socket)", self.repl_bootstrap_stalled_total());

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
        s.push_str("# HELP b2bua_requests_out_total outbound SIP requests this worker ORIGINATED/relayed by method (e.g. the in-dialog keepalive OPTIONS); pair with b2bua_responses_total{method=\"OPTIONS\",code=\"200\"} to see the keepalive round-trip\n# TYPE b2bua_requests_out_total counter\n");
        for (method, v) in self.inner.requests_out.lock().unwrap().iter() {
            s.push_str(&format!("b2bua_requests_out_total{{method=\"{method}\"}} {v}\n"));
        }
        s.push_str("# HELP b2bua_repl_applied_total inbound replication ops applied per stream+endpoint+op (flow=recovery|backup, peer=endpoint, op=create|update|delete); a reboot's bulk reclaim shows as a recovery/create step\n# TYPE b2bua_repl_applied_total counter\n");
        for (k, v) in self.inner.repl_applied.lock().unwrap().iter() {
            let mut p = k.splitn(3, '|');
            let flow = p.next().unwrap_or("");
            let peer = p.next().unwrap_or("");
            let op = p.next().unwrap_or("");
            s.push_str(&format!("b2bua_repl_applied_total{{flow=\"{flow}\",peer=\"{peer}\",op=\"{op}\"}} {v}\n"));
        }
        s.push_str("# HELP b2bua_repl_noops_sent_total catch-up/idle Noops sent per serve-side stream (flow=reclaim|backup, peer=caller); climbs continuously on a healthy stream — the backup-holder's 'sent everything in this flow' liveness sign (ADR-0014)\n# TYPE b2bua_repl_noops_sent_total counter\n");
        for (k, v) in self.inner.repl_noops_sent.lock().unwrap().iter() {
            let (flow, peer) = k.split_once('|').unwrap_or((k.as_str(), ""));
            s.push_str(&format!("b2bua_repl_noops_sent_total{{flow=\"{flow}\",peer=\"{peer}\"}} {v}\n"));
        }

        // Gauges last (direct writes — they end the `counter` closure's borrow).
        s.push_str("# HELP b2bua_active_calls live calls this worker is serving (creations - removals; now a true gauge since the two are paired)\n# TYPE b2bua_active_calls gauge\n");
        s.push_str(&format!("b2bua_active_calls {active}\n"));
        // (b2bua_repl_backup_held removed — the accurate resident backup count is
        // the sampled b2bua_repl_meta_backup gauge below.)
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
        g(&mut s, "b2bua_store_takeover_at", "live acting-backup takeover copies (ADR-0014; self-released on the served transaction's terminal state)", self.inner.store_takeover_at.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_touched", "last-touched ledger entries (reaper liveness stamps, ADR-0020; mirrors store_calls — a gap is a stamp leak)", self.inner.store_touched.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_total", "replica metadata entries held (all partitions)", self.inner.repl_meta_total.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_backup", "replica metadata entries in BACKUP partitions (resident backup bodies this node holds for peers; ADR-0014)", self.inner.repl_meta_backup.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_entries", "outbound changelog entries across all peer logs (replication buffer depth)", self.inner.repl_changelog_entries.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_peers", "peer logs currently held in the changelog", self.inner.repl_changelog_peers.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_bootstrap_last_applied", "bodies the most recent bootstrap pass imported (re-stalling at the same value across passes ⇒ the stream is truncating, not the materialisation)", self.repl_bootstrap_last_applied());
        g(&mut s, "b2bua_repl_reclaim_scanned", "bodies the most recent bulk reclaim pass found in pri:{self} (denominator: everything bootstrap import made reclaimable; ≪ peer repl_meta_backup ⇒ a bootstrap-import/forward-replication gap)", self.repl_reclaim_scanned());
        g(&mut s, "b2bua_repl_reclaim_materialized", "bodies the most recent bulk reclaim pass freshly re-served into the live map (cumulative total is repl_reclaimed_total; ≪ scanned cumulatively ⇒ a materialise gap)", self.repl_reclaim_materialized());
        // State-machine cursor census (ADR-0016 slice 9): live calls per
        // (machine,state). global-call is always present (Active/Terminating);
        // transfer/announcement appear only while a service is active — a labelled
        // gauge, so a drained cursor simply stops being emitted.
        s.push_str("# HELP b2bua_sm_cursors live calls resting at each state-machine cursor (machine=global-call|transfer|announcement|…, state=label); the live distribution of every call's machine positions (ADR-0016)\n# TYPE b2bua_sm_cursors gauge\n");
        for (k, v) in self.inner.sm_cursors.lock().unwrap().iter() {
            let (machine, state) = k.split_once('|').unwrap_or((k.as_str(), ""));
            s.push_str(&format!("b2bua_sm_cursors{{machine=\"{machine}\",state=\"{state}\"}} {v}\n"));
        }
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

        m.set_store_gauges(7, 11, 7, 9, 3, 7);
        m.set_repl_store_gauges(40, 22, 64, 4);
        let txt = m.prometheus_text();
        // A store_locks (9) > store_calls (7) gap is exactly the lock-leak signal.
        assert!(txt.contains("b2bua_store_calls 7"));
        assert!(txt.contains("b2bua_store_sip_index 11"));
        assert!(txt.contains("b2bua_store_indexed 7"));
        assert!(txt.contains("b2bua_store_locks 9"));
        assert!(txt.contains("b2bua_store_takeover_at 3"));
        assert!(txt.contains("b2bua_store_touched 7"));
        assert!(txt.contains("b2bua_repl_meta_total 40"));
        assert!(txt.contains("b2bua_repl_meta_backup 22"));
        assert!(txt.contains("b2bua_repl_changelog_entries 64"));
        assert!(txt.contains("b2bua_repl_changelog_peers 4"));
        // Each gauge series must carry its TYPE line (Prometheus exposition).
        assert!(txt.contains("# TYPE b2bua_store_calls gauge"));
        assert!(txt.contains("# TYPE b2bua_repl_meta_backup gauge"));
    }

    #[test]
    fn sm_cursor_census_renders_and_overwrites() {
        let m = B2buaMetrics::new();
        // Unset → the gauge family is declared but emits no series.
        let zero = m.prometheus_text();
        assert!(zero.contains("# TYPE b2bua_sm_cursors gauge"));
        assert!(!zero.contains("b2bua_sm_cursors{"));

        let mut census = BTreeMap::new();
        census.insert(("global-call".to_string(), "Active".to_string()), 5);
        census.insert(("transfer".to_string(), "CRinging".to_string()), 2);
        m.set_sm_cursor_census(census);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_sm_cursors{machine=\"global-call\",state=\"Active\"} 5"));
        assert!(txt.contains("b2bua_sm_cursors{machine=\"transfer\",state=\"CRinging\"} 2"));

        // A fresh census OVERWRITES: a cursor that drained to zero disappears
        // rather than sticking at its last value (gauge, not counter).
        let mut next = BTreeMap::new();
        next.insert(("global-call".to_string(), "Active".to_string()), 3);
        m.set_sm_cursor_census(next);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_sm_cursors{machine=\"global-call\",state=\"Active\"} 3"));
        assert!(!txt.contains("machine=\"transfer\""));
    }
}
