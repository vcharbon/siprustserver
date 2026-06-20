//! B2BUA metrics — atomic counters/gauges (the source's `MetricsRegistry`
//! surface reduced to the counters the ported paths move). Cheap to clone
//! (one `Arc`); read with the `*_total` accessors.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::tier1_brake::Tier1BrakeCounters;

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
    // Tier-3 admission gate (migration/09): new INVITEs the worker shed with a
    // stateless 503 because the hard CPS token bucket was empty OR the worker's
    // EWMA-ELU exceeded the panic backstop. A climbing rate is the worker
    // protecting itself from new-call overload (the LB's AIMD should have shed
    // first; a non-zero local count flags the LB absent/misconfigured/overloaded).
    overload_rejected: AtomicU64,
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
    // Inner CallStore map sizes (bodies/idx) — the idx map is insert-only on
    // put_call, so a call whose index keys change across re-flushes (or whose
    // delete passes the wrong keys) strands `idx:*` entries. store_idx_entries
    // climbing while store_bodies is flat is THAT leak (the no-chaos RSS climb).
    store_bodies: AtomicU64,
    store_idx_entries: AtomicU64,
    store_tombstones: AtomicU64,
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
    // Per-call Vec census (sampled, summed across the live call map under the
    // store lock). The count-gauges above bound the MAP sizes; these bound the
    // BYTES held *inside* each call. A 10h NO_CHAOS soak showed jemalloc
    // `allocated` climbing ~135 MB/h with EVERY map count flat — the leak is a
    // per-call Vec that grows per in-dialog event on a long-held (OPTIONS-hold /
    // re-INVITE) dialog and is never pruned until terminal. These sums name it:
    // the one whose ratio over `store_calls` climbs is the leaking Vec.
    // `*_max` is the worst single call (a held dialog's unbounded tail).
    census_cdr_events: AtomicU64,
    census_pending_requests: AtomicU64,
    census_pending_requests_max: AtomicU64,
    census_dialogs: AtomicU64,
    census_route_set: AtomicU64,
    census_timers: AtomicU64,
    census_tag_map: AtomicU64,
    census_b_legs: AtomicU64,
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
    // Tier-3 admission gate (migration/09).
    counter!(bump_overload_rejected, overload_rejected_total, overload_rejected);
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

    /// Push the per-call Vec census (summed across the live call map). Sampled
    /// alongside `set_store_gauges` under the same store lock. The sum whose
    /// ratio over `store_calls` climbs while the count-gauges stay flat names the
    /// leaking per-call Vec (the bytes-inside-each-call leak the count-gauges
    /// cannot see).
    #[allow(clippy::too_many_arguments)]
    /// Push the inner CallStore map sizes (sampled in the runner's reap loop).
    pub fn set_store_map_sizes(&self, bodies: u64, idx_entries: u64, tombstones: u64) {
        self.inner.store_bodies.store(bodies, Ordering::Relaxed);
        self.inner.store_idx_entries.store(idx_entries, Ordering::Relaxed);
        self.inner.store_tombstones.store(tombstones, Ordering::Relaxed);
    }

    pub fn set_call_census(
        &self,
        cdr_events: u64,
        pending_requests: u64,
        pending_requests_max: u64,
        dialogs: u64,
        route_set: u64,
        timers: u64,
        tag_map: u64,
        b_legs: u64,
    ) {
        self.inner.census_cdr_events.store(cdr_events, Ordering::Relaxed);
        self.inner.census_pending_requests.store(pending_requests, Ordering::Relaxed);
        self.inner.census_pending_requests_max.store(pending_requests_max, Ordering::Relaxed);
        self.inner.census_dialogs.store(dialogs, Ordering::Relaxed);
        self.inner.census_route_set.store(route_set, Ordering::Relaxed);
        self.inner.census_timers.store(timers, Ordering::Relaxed);
        self.inner.census_tag_map.store(tag_map, Ordering::Relaxed);
        self.inner.census_b_legs.store(b_legs, Ordering::Relaxed);
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
        // ── Tier-3 admission gate (migration/09) ──
        counter("b2bua_overload_rejected_total", "new INVITEs shed with a stateless 503 by the Tier-3 admission gate (CPS token bucket empty OR panic-ELU backstop tripped; a non-zero rate flags the LB's AIMD absent/misconfigured/overloaded)", self.overload_rejected_total());
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
        g(&mut s, "b2bua_store_bodies", "inner CallStore body entries (pri:+bak: across partitions)", self.inner.store_bodies.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_idx_entries", "inner CallStore idx:* routing entries; outgrowing store_bodies = stranded-index leak (put_call is insert-only)", self.inner.store_idx_entries.load(Ordering::Relaxed));
        g(&mut s, "b2bua_store_tombstones", "resurrection-guard tombstones; outgrowing 300s×delete_rate = prune gap", self.inner.store_tombstones.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_total", "replica metadata entries held (all partitions)", self.inner.repl_meta_total.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_meta_backup", "replica metadata entries in BACKUP partitions (resident backup bodies this node holds for peers; ADR-0014)", self.inner.repl_meta_backup.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_entries", "outbound changelog entries across all peer logs (replication buffer depth)", self.inner.repl_changelog_entries.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_changelog_peers", "peer logs currently held in the changelog", self.inner.repl_changelog_peers.load(Ordering::Relaxed));
        g(&mut s, "b2bua_repl_bootstrap_last_applied", "bodies the most recent bootstrap pass imported (re-stalling at the same value across passes ⇒ the stream is truncating, not the materialisation)", self.repl_bootstrap_last_applied());
        g(&mut s, "b2bua_repl_reclaim_scanned", "bodies the most recent bulk reclaim pass found in pri:{self} (denominator: everything bootstrap import made reclaimable; ≪ peer repl_meta_backup ⇒ a bootstrap-import/forward-replication gap)", self.repl_reclaim_scanned());
        g(&mut s, "b2bua_repl_reclaim_materialized", "bodies the most recent bulk reclaim pass freshly re-served into the live map (cumulative total is repl_reclaimed_total; ≪ scanned cumulatively ⇒ a materialise gap)", self.repl_reclaim_materialized());
        // Per-call Vec census: bytes-inside-each-call. The sum whose ratio over
        // store_calls climbs while every count-gauge is flat names the leaking
        // per-call Vec (a held dialog's per-event tail never pruned till terminal).
        g(&mut s, "b2bua_census_cdr_events", "sum of cdr_events Vec len across live calls (drained only at terminal; climbing ratio vs store_calls = per-call CDR leak)", self.inner.census_cdr_events.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_pending_requests", "sum of inbound_pending_requests across all dialogs of live calls (removed only on a correlated final response; a climbing ratio = uncorrelated/lost-response leak)", self.inner.census_pending_requests.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_pending_requests_max", "max inbound_pending_requests on any single live call (the worst held dialog)", self.inner.census_pending_requests_max.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_dialogs", "sum of dialogs Vec len across all legs of live calls (forking early-dialogs should collapse to 1 after confirm; a climb = un-pruned fork)", self.inner.census_dialogs.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_route_set", "sum of dialog route_set entries across live calls", self.inner.census_route_set.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_timers", "sum of serializable timer-intent Vec len across live calls (deduped by id; should be flat per call)", self.inner.census_timers.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_tag_map", "sum of tag_map entries across live calls", self.inner.census_tag_map.load(Ordering::Relaxed));
        g(&mut s, "b2bua_census_b_legs", "sum of b_legs Vec len across live calls", self.inner.census_b_legs.load(Ordering::Relaxed));
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

// ---------------------------------------------------------------------------
// UdpTransportMetrics — the `UdpTransport` facade's Prometheus-visible shape
// ---------------------------------------------------------------------------

/// BufferedUdpEndpoint counters — non-blocking outbound send (port of
/// `BufferedUdpEndpoint.ts`'s `BufferedSendCounters`). Clone-cheap (one `Arc`);
/// every field is a shared lock-free atomic so the per-peer drainer fiber's hot
/// path stays cheap and the `/metrics` scrape reads them without a lock.
///
/// **Status:** the value *shape* (the six counters) is retained here so the
/// [`UdpTransportMetrics`] surface StatusServer/Prometheus expects is complete
/// and stable. The *producer* — the `wrapEndpoint` per-peer outbound drainer —
/// was **removed (won't port)**: it guarded against a blocking `getaddrinfo` in
/// Node's `send`, which has no analogue in tokio (sends take an already-resolved
/// `SocketAddr`, so there is nothing to quarantine), and per-peer queuing buys
/// no isolation for real UDP. The b2bua-runner sends straight through the raw
/// `UdpEndpoint`, so these counters are **permanently zero** — a flat, declared
/// series rather than a missing one, exactly as an un-wrapped TS transport would
/// render (`bufferedSendPerPeerQueueMax === 0` → wrapper disabled, counters 0).
/// The fields are kept (vs. deleted) only to keep the metric/dashboard series
/// stable; a later cleanup may drop them along with the dashboard panels.
#[derive(Debug, Clone, Default)]
pub struct BufferedSendCounters {
    inner: Arc<BufferedSendInner>,
}

#[derive(Debug, Default)]
struct BufferedSendInner {
    enqueued: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_evicted_with_queue: AtomicU64,
    inner_send_errors: AtomicU64,
    reclaimed_idle: AtomicU64,
    reclaimed_cap: AtomicU64,
}

impl BufferedSendCounters {
    /// Fresh counters at zero (the TS `makeBufferedSendCounters()`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Items accepted into a per-peer queue (`enqueued`).
    pub fn enqueued(&self) -> u64 {
        self.inner.enqueued.load(Ordering::Relaxed)
    }
    /// Items dropped because the target peer's queue was full — drop-newest,
    /// matching kernel UDP (`droppedQueueFull`).
    pub fn dropped_queue_full(&self) -> u64 {
        self.inner.dropped_queue_full.load(Ordering::Relaxed)
    }
    /// Items still queued on a peer entry when it was evicted (idle/cap reclaim),
    /// counted as dropped (`droppedEvictedWithQueue`).
    pub fn dropped_evicted_with_queue(&self) -> u64 {
        self.inner.dropped_evicted_with_queue.load(Ordering::Relaxed)
    }
    /// Inner `send` failures the drainer swallowed (SIP UDP retransmits cover
    /// the loss) (`innerSendErrors`).
    pub fn inner_send_errors(&self) -> u64 {
        self.inner.inner_send_errors.load(Ordering::Relaxed)
    }
    /// Peer entries reclaimed for idleness (no successful drain within the TTL)
    /// (`reclaimedIdle`).
    pub fn reclaimed_idle(&self) -> u64 {
        self.inner.reclaimed_idle.load(Ordering::Relaxed)
    }
    /// Peer entries evicted to make room under the max-peers ceiling
    /// (`reclaimedCap`).
    pub fn reclaimed_cap(&self) -> u64 {
        self.inner.reclaimed_cap.load(Ordering::Relaxed)
    }

    // --- write side (used by the future BufferedUdpEndpoint drainer) ---
    /// `enqueued++`.
    pub fn record_enqueued(&self) {
        self.inner.enqueued.fetch_add(1, Ordering::Relaxed);
    }
    /// `droppedQueueFull++`.
    pub fn record_dropped_queue_full(&self) {
        self.inner.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
    }
    /// `droppedEvictedWithQueue += n` (a whole queue's worth at eviction).
    pub fn add_dropped_evicted_with_queue(&self, n: u64) {
        self.inner.dropped_evicted_with_queue.fetch_add(n, Ordering::Relaxed);
    }
    /// `innerSendErrors++`.
    pub fn record_inner_send_error(&self) {
        self.inner.inner_send_errors.fetch_add(1, Ordering::Relaxed);
    }
    /// `reclaimedIdle++`.
    pub fn record_reclaimed_idle(&self) {
        self.inner.reclaimed_idle.fetch_add(1, Ordering::Relaxed);
    }
    /// `reclaimedCap++`.
    pub fn record_reclaimed_cap(&self) {
        self.inner.reclaimed_cap.fetch_add(1, Ordering::Relaxed);
    }
}

/// A live-read source for an endpoint gauge (`queueDepth`, `dropsTailDrop`,
/// `bufferedSendPeerCount`). `Arc<dyn Fn>` so the surface is decoupled from the
/// concrete `UdpEndpoint` type (which is held as a `Box<dyn UdpEndpoint>` by the
/// runner and is not `Clone`); the closure captures a clone of the shared
/// counter/queue handle and reads it on demand. `Send + Sync` because the
/// `/metrics` scrape may run on any task.
pub type LiveGauge = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The `UdpTransport` facade's Prometheus-visible shape — a faithful port of
/// `UdpTransportMetrics` (`src/sip/UdpTransport.ts`). The TS interface is a bag
/// of **live getters** (`get queueDepth() { return endpoint.queueDepth() }`,
/// etc.): both the scrape endpoint and test reads want the *instantaneous*
/// value, never a cached snapshot. This Rust port preserves that — every facet
/// reads through on each access:
///
///   - `queue_depth` / `drops_tail_drop` → injected [`LiveGauge`]s backed by the
///     underlying [`UdpEndpoint`] (`endpoint.queueDepth()` /
///     `endpoint.counters().tail_dropped`).
///   - `queue_max` → the bind's configured bound (a constant, copied once).
///   - `drops_tier1_brake` / `tier1_reject_sent` → the shared
///     [`Tier1BrakeCounters`] the `preIngress` hook mutates.
///   - `buffered_send` → the [`BufferedSendCounters`] (zero until the
///     `BufferedUdpEndpoint` drainer is ported — see that type's note).
///   - `buffered_send_peer_count` → an injected [`LiveGauge`] for the wrapped
///     endpoint's active per-peer drainer count (`peerCount()`); `|| 0` until
///     the wrapper exists, matching the TS `wrappedEndpoint?.peerCount() ?? 0`.
///
/// Clone-cheap (all fields are `Arc`/`Copy`): the runner keeps one to render
/// `/metrics` and may hand clones to other readers. This is the registry-side of
/// the TS `registry.udp = metrics` assignment — the runner builds it from the
/// bound endpoint + brake counters and concatenates [`Self::prometheus_text`]
/// into the `/metrics` body.
#[derive(Clone)]
pub struct UdpTransportMetrics {
    queue_depth: LiveGauge,
    queue_max: usize,
    drops_tail_drop: LiveGauge,
    brake: Tier1BrakeCounters,
    buffered_send: BufferedSendCounters,
    buffered_send_peer_count: LiveGauge,
}

impl std::fmt::Debug for UdpTransportMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Closures aren't Debug — render the live values instead.
        f.debug_struct("UdpTransportMetrics")
            .field("queue_depth", &self.queue_depth())
            .field("queue_max", &self.queue_max)
            .field("drops_tier1_brake", &self.drops_tier1_brake())
            .field("drops_tail_drop", &self.drops_tail_drop())
            .field("tier1_reject_sent", &self.tier1_reject_sent())
            .field("buffered_send", &self.buffered_send)
            .field("buffered_send_peer_count", &self.buffered_send_peer_count())
            .finish()
    }
}

impl UdpTransportMetrics {
    /// Build the shape from its live sources — the registry-side of the TS
    /// `UdpTransport.layer` (`const metrics: UdpTransportMetrics = { … }`).
    ///
    ///   - `queue_max`: the bind's configured queue bound (`config.udpQueueMax`).
    ///   - `brake`: the [`Tier1BrakeCounters`] the `preIngress` hook holds (so
    ///     `drops_tier1_brake` / `tier1_reject_sent` read live).
    ///   - `queue_depth` / `drops_tail_drop`: live getters over the bound
    ///     endpoint (typically `move || endpoint.queue_depth()` and
    ///     `move || endpoint.counters().tail_dropped` with a shared handle).
    ///
    /// Buffered-send facets default to empty/zero — use
    /// [`with_buffered`](Self::with_buffered) once the `BufferedUdpEndpoint`
    /// drainer is ported.
    pub fn new(
        queue_max: usize,
        brake: Tier1BrakeCounters,
        queue_depth: LiveGauge,
        drops_tail_drop: LiveGauge,
    ) -> Self {
        Self {
            queue_depth,
            queue_max,
            drops_tail_drop,
            brake,
            buffered_send: BufferedSendCounters::new(),
            buffered_send_peer_count: Arc::new(|| 0),
        }
    }

    /// Attach the buffered-send counters + live per-peer count (the TS
    /// `bufferedSend` / `get bufferedSendPeerCount()`), once the
    /// `BufferedUdpEndpoint` wrapper is ported and wired. Until then the default
    /// from [`new`](Self::new) (empty counters, `|| 0`) is correct.
    pub fn with_buffered(
        mut self,
        buffered_send: BufferedSendCounters,
        buffered_send_peer_count: LiveGauge,
    ) -> Self {
        self.buffered_send = buffered_send;
        self.buffered_send_peer_count = buffered_send_peer_count;
        self
    }

    /// Live inbound-queue depth (`endpoint.queueDepth()`).
    pub fn queue_depth(&self) -> u64 {
        (self.queue_depth)()
    }
    /// The configured inbound-queue bound (`config.udpQueueMax`).
    pub fn queue_max(&self) -> usize {
        self.queue_max
    }
    /// New non-emergency INVITEs the Tier-1 brake shed (`dropsTier1Brake`).
    pub fn drops_tier1_brake(&self) -> u64 {
        self.brake.drops_tier1_brake()
    }
    /// Datagrams the full inbound queue tail-dropped (`dropsTailDrop`, live ←
    /// `endpoint.counters.tailDropped`).
    pub fn drops_tail_drop(&self) -> u64 {
        (self.drops_tail_drop)()
    }
    /// Stateless 503s the brake emitted (`tier1RejectSent`).
    pub fn tier1_reject_sent(&self) -> u64 {
        self.brake.tier1_reject_sent()
    }
    /// New emergency INVITEs that bypassed the Tier-1 brake above the threshold.
    pub fn tier1_emergency_bypassed(&self) -> u64 {
        self.brake.emergency_bypassed()
    }
    /// The non-blocking outbound-send counters (`bufferedSend`).
    pub fn buffered_send(&self) -> &BufferedSendCounters {
        &self.buffered_send
    }
    /// Active per-peer drainer fibers (`bufferedSendPeerCount`).
    pub fn buffered_send_peer_count(&self) -> u64 {
        (self.buffered_send_peer_count)()
    }

    /// Render the shape as Prometheus text exposition for the `/metrics` body —
    /// the registry-visible surface of the TS `registry.udp = metrics`
    /// assignment. All series use the `b2bua_udp_*` namespace.
    ///
    /// Counters (monotonic): `tier1_brake_drops`, `tier1_reject_sent`,
    /// `tail_dropped`, and the six `buffered_send_*`. Gauges (instantaneous):
    /// `queue_depth`, `queue_max`, `buffered_send_peers`. The two brake counters
    /// keep their existing standalone names (`b2bua_udp_tier1_brake_drops_total`
    /// / `b2bua_udp_tier1_reject_sent_total`) so dashboards built against the
    /// brake item keep working — this shape *supersedes* the runner's old
    /// `tier1_brake_metrics_text` by rendering the same two lines plus the
    /// queue/tail-drop/buffered facets.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(1536);
        let counter = |s: &mut String, name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"));
        };
        let gauge = |s: &mut String, name: &str, help: &str, v: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
        };

        // ── Tier-1 brake (port of UdpTransportMetrics.dropsTier1Brake /
        //    tier1RejectSent). Names unchanged from the brake item. ──
        counter(
            &mut s,
            "b2bua_udp_tier1_brake_drops_total",
            "New non-emergency INVITEs shed at the UDP ingress by the Tier-1 overload brake (queue depth crossed floor(queue_max*pct/100)); cheapest stateless-503 shed, ahead of the Tier-3 admission gate.",
            self.drops_tier1_brake(),
        );
        counter(
            &mut s,
            "b2bua_udp_tier1_reject_sent_total",
            "Stateless 503 (Service Unavailable + Retry-After) responses the Tier-1 brake emitted back to the source. Moves in lockstep with the drops counter.",
            self.tier1_reject_sent(),
        );
        counter(
            &mut s,
            "b2bua_udp_tier1_emergency_bypassed_total",
            "New emergency INVITEs that crossed the Tier-1 threshold but BYPASSED the brake (always admitted). Emergency traffic skipping the gate under flood.",
            self.tier1_emergency_bypassed(),
        );

        // ── Inbound queue state (port of UdpTransportMetrics.queueDepth /
        //    queueMax / dropsTailDrop — live getters over the endpoint). A
        //    tail-dropping queue otherwise shows 100% accepted (the blind spot
        //    that hid the 2026-06-12 burst collapse on the proxy side). ──
        gauge(
            &mut s,
            "b2bua_udp_queue_depth",
            "Live inbound UDP queue depth (port of UdpTransportMetrics.queueDepth).",
            self.queue_depth(),
        );
        gauge(
            &mut s,
            "b2bua_udp_queue_max",
            "Configured inbound UDP queue bound (udpQueueMax).",
            self.queue_max() as u64,
        );
        counter(
            &mut s,
            "b2bua_udp_tail_dropped_total",
            "Datagrams tail-dropped by the full inbound queue (port of UdpTransportMetrics.dropsTailDrop).",
            self.drops_tail_drop(),
        );

        // ── Buffered (non-blocking) outbound send (port of
        //    UdpTransportMetrics.bufferedSend / bufferedSendPeerCount). Zero
        //    until the BufferedUdpEndpoint drainer is ported — a flat declared
        //    series, not a missing one. ──
        let b = &self.buffered_send;
        counter(&mut s, "b2bua_udp_buffered_send_enqueued_total", "Outbound datagrams accepted into a per-peer buffered-send queue.", b.enqueued());
        counter(&mut s, "b2bua_udp_buffered_send_dropped_queue_full_total", "Outbound datagrams dropped because the target peer's buffered-send queue was full (drop-newest).", b.dropped_queue_full());
        counter(&mut s, "b2bua_udp_buffered_send_dropped_evicted_total", "Outbound datagrams still queued when their peer entry was evicted (idle/cap reclaim).", b.dropped_evicted_with_queue());
        counter(&mut s, "b2bua_udp_buffered_send_inner_errors_total", "Inner socket-send failures the per-peer drainer swallowed (SIP UDP retransmits cover the loss).", b.inner_send_errors());
        counter(&mut s, "b2bua_udp_buffered_send_reclaimed_idle_total", "Per-peer buffered-send entries reclaimed for idleness (no successful drain within the TTL).", b.reclaimed_idle());
        counter(&mut s, "b2bua_udp_buffered_send_reclaimed_cap_total", "Per-peer buffered-send entries evicted to stay under the max-peers ceiling.", b.reclaimed_cap());
        gauge(
            &mut s,
            "b2bua_udp_buffered_send_peers",
            "Active per-peer buffered-send drainer fibers (port of UdpTransportMetrics.bufferedSendPeerCount).",
            self.buffered_send_peer_count(),
        );
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

    // -----------------------------------------------------------------------
    // UdpTransportMetrics shape (port of `UdpTransport.ts` UdpTransportMetrics)
    // -----------------------------------------------------------------------

    /// A `UdpTransportMetrics` whose `queueDepth` / `dropsTailDrop` are backed by
    /// caller-held atomics (standing in for a live `UdpEndpoint`), with the real
    /// brake counters and (default) empty buffered-send. Mirrors the TS shape's
    /// live getters without standing up a fabric — the fabric-proxy facet is
    /// covered end-to-end in `tests/udp_transport_metrics.rs`.
    fn shape() -> (UdpTransportMetrics, Tier1BrakeCounters, Arc<AtomicU64>, Arc<AtomicU64>) {
        let brake = Tier1BrakeCounters::new();
        let depth = Arc::new(AtomicU64::new(0));
        let tail = Arc::new(AtomicU64::new(0));
        let d = depth.clone();
        let t = tail.clone();
        let m = UdpTransportMetrics::new(
            5, // queue_max — the brake test's QUEUE_MAX
            brake.clone(),
            Arc::new(move || d.load(Ordering::Relaxed)),
            Arc::new(move || t.load(Ordering::Relaxed)),
        );
        (m, brake, depth, tail)
    }

    /// Every facet is a LIVE getter: a value changed in the underlying source is
    /// seen by the metrics shape on the next read, never a stale snapshot. This
    /// is the TS `get queueDepth() { return endpoint.queueDepth() }` contract.
    #[test]
    fn udp_transport_metrics_facets_are_live() {
        let (m, brake, depth, tail) = shape();
        // All zero initially.
        assert_eq!(m.queue_depth(), 0);
        assert_eq!(m.queue_max(), 5);
        assert_eq!(m.drops_tier1_brake(), 0);
        assert_eq!(m.drops_tail_drop(), 0);
        assert_eq!(m.tier1_reject_sent(), 0);
        assert_eq!(m.buffered_send_peer_count(), 0);

        // Mutate the sources — the shape reflects them live.
        depth.store(2, Ordering::Relaxed);
        tail.store(7, Ordering::Relaxed);
        brake.record_shed();
        brake.record_shed();
        brake.record_shed();
        assert_eq!(m.queue_depth(), 2);
        assert_eq!(m.drops_tail_drop(), 7);
        // dropsTier1Brake / tier1RejectSent move in lockstep (one 503 per shed).
        assert_eq!(m.drops_tier1_brake(), 3);
        assert_eq!(m.tier1_reject_sent(), 3);
    }

    /// The metrics-shape facet of `UdpTransport-brake.test.ts`'s first case
    /// ("non-emergency INVITEs past the threshold receive a stateless 503"):
    /// after the brake sheds `floodCount - 2` INVITEs into an undrained queue,
    /// `udp.metrics.{dropsTier1Brake,tier1RejectSent}` == the shed count and
    /// `udp.metrics.queueDepth` == 2 (the two below-threshold INVITEs that were
    /// enqueued). Here the brake counters are driven directly and the queue depth
    /// gauge is set to the enqueued count; the fabric-driven version is in
    /// `tests/udp_transport_metrics.rs`.
    #[test]
    fn udp_transport_metrics_matches_brake_test_shape() {
        let (m, brake, depth, _tail) = shape();
        let flood = 10u64;
        // Two enqueued (depth 0,1 accepted), `flood - 2` shed at depth >= 2.
        depth.store(2, Ordering::Relaxed);
        for _ in 0..(flood - 2) {
            brake.record_shed();
        }
        assert_eq!(m.drops_tier1_brake(), flood - 2);
        assert_eq!(m.tier1_reject_sent(), flood - 2);
        assert_eq!(m.queue_depth(), 2);
    }

    /// The render carries every field of the shape, with the right Prometheus
    /// TYPE per field (counters for the monotonic sheds/tail-drops/buffered,
    /// gauges for the instantaneous depth/max/peers), and keeps the brake item's
    /// existing metric names so dashboards transfer.
    #[test]
    fn udp_transport_metrics_render() {
        let (m, brake, depth, tail) = shape();
        depth.store(3, Ordering::Relaxed);
        tail.store(11, Ordering::Relaxed);
        brake.record_shed();
        let txt = m.prometheus_text();

        // Brake counters — names unchanged from the standalone brake item.
        assert!(txt.contains("b2bua_udp_tier1_brake_drops_total 1"));
        assert!(txt.contains("b2bua_udp_tier1_reject_sent_total 1"));
        // Emergency-bypass visibility series (renders even at 0).
        assert!(txt.contains("b2bua_udp_tier1_emergency_bypassed_total 0"));
        assert!(txt.contains("# TYPE b2bua_udp_tier1_emergency_bypassed_total counter"));
        // Live queue facets.
        assert!(txt.contains("b2bua_udp_queue_depth 3"));
        assert!(txt.contains("b2bua_udp_queue_max 5"));
        assert!(txt.contains("b2bua_udp_tail_dropped_total 11"));
        // Buffered-send facets render at zero (drainer not yet ported).
        assert!(txt.contains("b2bua_udp_buffered_send_enqueued_total 0"));
        assert!(txt.contains("b2bua_udp_buffered_send_dropped_queue_full_total 0"));
        assert!(txt.contains("b2bua_udp_buffered_send_peers 0"));
        // Prometheus TYPE lines: gauges vs counters.
        assert!(txt.contains("# TYPE b2bua_udp_queue_depth gauge"));
        assert!(txt.contains("# TYPE b2bua_udp_queue_max gauge"));
        assert!(txt.contains("# TYPE b2bua_udp_buffered_send_peers gauge"));
        assert!(txt.contains("# TYPE b2bua_udp_tail_dropped_total counter"));
        assert!(txt.contains("# TYPE b2bua_udp_buffered_send_enqueued_total counter"));
    }

    /// The buffered-send counters carry the full TS `BufferedSendCounters` shape
    /// (six fields) and are live + attachable via `with_buffered` — the seam the
    /// future `BufferedUdpEndpoint` drainer writes through (and `peerCount()`
    /// feeds `bufferedSendPeerCount`). Today nothing produces them, so the
    /// default is all-zero; this pins the shape + the attach seam.
    #[test]
    fn buffered_send_counters_shape_and_attach() {
        let (m0, ..) = shape();
        // Default: empty counters, zero peer count (TS un-wrapped transport).
        assert_eq!(m0.buffered_send().enqueued(), 0);
        assert_eq!(m0.buffered_send_peer_count(), 0);

        // Attach a populated counter set + a live peer-count source.
        let bc = BufferedSendCounters::new();
        bc.record_enqueued();
        bc.record_enqueued();
        bc.record_dropped_queue_full();
        bc.add_dropped_evicted_with_queue(4);
        bc.record_inner_send_error();
        bc.record_reclaimed_idle();
        bc.record_reclaimed_cap();
        let peers = Arc::new(AtomicU64::new(2));
        let p = peers.clone();
        let m = m0.with_buffered(bc.clone(), Arc::new(move || p.load(Ordering::Relaxed)));

        assert_eq!(m.buffered_send().enqueued(), 2);
        assert_eq!(m.buffered_send().dropped_queue_full(), 1);
        assert_eq!(m.buffered_send().dropped_evicted_with_queue(), 4);
        assert_eq!(m.buffered_send().inner_send_errors(), 1);
        assert_eq!(m.buffered_send().reclaimed_idle(), 1);
        assert_eq!(m.buffered_send().reclaimed_cap(), 1);
        assert_eq!(m.buffered_send_peer_count(), 2);

        // The render now reflects the attached values, and is still live: a
        // post-build mutation of the shared handle is visible.
        peers.store(5, Ordering::Relaxed);
        bc.record_enqueued();
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_udp_buffered_send_enqueued_total 3"));
        assert!(txt.contains("b2bua_udp_buffered_send_dropped_evicted_total 4"));
        assert!(txt.contains("b2bua_udp_buffered_send_peers 5"));
    }
}
