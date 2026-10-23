//! B2BUA metrics — atomic counters/gauges. Cheap to clone (one `Arc`); read
//! with the `*_total` accessors.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use metric_catalogue::HistogramValue;
use metric_values::{FixedCounts, OpenRows};

use crate::dispatch::{Discard, PastBound};
use crate::drain::DrainExit;
use catalogue::worker::{DRAIN_BUCKETS, PAST_BOUND, REMOVAL_CLASS};

pub mod catalogue;
mod cells;
mod limiter;
mod operator;
mod removal_class;
mod retransmit_row;
pub mod shell;
mod turn_counts;
mod udp_transport;
mod wire_drops;

pub use limiter::{
    AdmitSite, LimiterCounters, LimiterFailure, LimiterOp, LimiterTask, RefreshDiscard,
    RefreshGiveUp, ReleaseGiveUp,
};
pub use operator::OPERATOR_RELEASE_KINDS;
pub use removal_class::RemovalClass;
use retransmit_row::retransmit_row;
pub use udp_transport::{LiveCounters, LiveGauge, UdpTransportMetrics};

#[derive(Debug, Default)]
struct Inner {
    plain: catalogue::plain::PlainCounters,
    // The semi-open families (per-method traffic, repeats and give-ups,
    // unroutable messages, replication per stream, the cursor census), one
    // row per observed label set.
    rows: Rows,
    // Why each drain returned, by reason (`quiescent`, `caught_up`,
    // `grace`, `grace_peers_behind` — ADR-0031 D2). `grace_peers_behind` means a
    // departing worker abandoned live calls no peer reported holding: a lost
    // flush window, the one reason that must never be read as a clean drain.
    drain_exits: [AtomicU64; DrainExit::ALL.len()],
    // Time-in-drain: fixed-bucket cumulative counts (`le` in seconds), the sum in
    // milliseconds and the observation count.
    drain_seconds_buckets: [AtomicU64; catalogue::worker::DRAIN_BUCKETS.len()],
    drain_seconds_sum_ms: AtomicU64,
    drain_seconds_count: AtomicU64,
    // What every operator release-all reached, by kind (ADR-0050 §10).
    operator_releases: operator::OperatorReleases,
    // The worker's view of its call limiter (ADR-0040).
    limiter: LimiterCounters,
    past_bound_depth: AtomicU64,
    past_bound_cap: AtomicU64,
    overflow_refused: AtomicU64,
    overflow_depth: std::sync::atomic::AtomicI64,
    invite_discard_answered_queue_full: AtomicU64,
    invite_discard_answered_at_cap: AtomicU64,
    invite_discard_answered_released: AtomicU64,
    invite_discard_answered_capped: AtomicU64,
    calls_near_lifetime_cap: std::sync::atomic::AtomicI64,
    creations: AtomicU64,
    removals: AtomicU64,
    removals_terminated: AtomicU64,
    removals_self_release: AtomicU64,
    removals_stand_down: AtomicU64,
    removals_orphan: AtomicU64,
    // timer service (gauges): physical DelayQueue size (live entries + not-yet-
    // expired tombstones from cancelled/rescheduled timers) vs. the live
    // schedulable timer count. `queue_len - live` is the lingering-tombstone
    // backlog — the work that grows with cancelled long-interval timers
    // (e.g. the per-call 1 h GlobalDuration) even while active_calls is flat.
    timer_queue_len: AtomicU64,
    timer_live: AtomicU64,
    // Clock-skew divergence (clock-skew hardening): the SIGNED gap between a fresh
    // raw `SystemTime` reading and this node's monotonic-anchored `Clock::now_ms()`
    // (`raw_wall − now_ms`), sampled ~every 30 s. `now_ms` does not follow a host
    // NTP STEP (it rides the monotonic clock), so a large sudden magnitude names
    // the exact event that skews replicated timer deadlines across pods — a live
    // signal rather than a post-mortem finding. Stored as the two's-complement
    // bits of an i64 (Prometheus has no signed atomic); the render reinterprets.
    // NOT a re-anchor trigger — the behavioural correction is the
    // replication-boundary re-anchor, not a clock rewrite (timestamps stay
    // monotonic).
    clock_wall_divergence_ms: AtomicU64,
    // Re-hydration diagnostics. How a rebooted primary's bootstrap passes
    // terminate: `seeded` = a pass reached the first catch-up `Noop` (the peer
    // streamed the full `bak:{me}` keyset); `stalled` = a pass hit the bootstrap
    // hard deadline before that Noop arrived (marked complete best-effort,
    // partial pre-seed materialised, then KEEPS streaming on the same socket —
    // not a disconnect). `last_applied` (gauge) = bodies the MOST RECENT pass
    // imported. The decisive signal: if `stalled` climbs and `last_applied` keeps
    // re-stalling at the SAME value across passes, the STREAM is truncating (a
    // peer-side stall), not just the materialisation — and a longer hard deadline
    // alone would not help. If `seeded` bumps and `repl_reclaimed_total` ≈ the
    // held count, re-hydration is whole.
    repl_bootstrap_last_applied: AtomicU64,
    // Reboot-reclaim completeness. Per the MOST RECENT bulk reclaim pass
    // (`router::reclaim_all`): `scanned` = bodies found in `pri:{self}` (the
    // denominator — everything the bootstrap import made reclaimable on this
    // node) and `materialized` = how many of those this pass freshly inserted
    // into the live serving map + re-armed timers. The per-reboot chain localises
    // exactly where a rebooted primary's quiescent dialogs are lost: `(peer)
    // repl_meta_backup` → `repl_bootstrap_last_applied` → `repl_reclaim_scanned`
    // → `repl_reclaim_materialized`. `scanned ≪ peer meta_backup` ⇒ a
    // bootstrap-import / forward-replication gap; `materialized ≪ scanned`
    // (cumulatively, via `repl_reclaimed_total`) ⇒ a materialise gap.
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
    // climbing while store_bodies is flat is THAT leak.
    store_bodies: AtomicU64,
    store_idx_entries: AtomicU64,
    store_tombstones: AtomicU64,
    repl_changelog_entries: AtomicU64,
    repl_changelog_peers: AtomicU64,
    // 1 while this worker has observed its own endpoint withdrawn from routing
    // and the process still runs (ADR-0031 D6): the drain that SIGTERM has not
    // (yet) ended. Sticky for the life of the process.
    withdrawn_running: AtomicU64,
    // Replication peers pulled while their endpoint is not `ready` (present in
    // membership only — a terminating or flapping member, ADR-0031 D1). Sampled
    // at every supervisor reconcile. Non-zero for longer than a drain grace names
    // a member stuck `Terminating` or a readiness probe that will not recover.
    repl_peers_pulled_not_ready: AtomicU64,
    // Per-call Vec census (sampled, summed across the live call map under the
    // store lock). The count-gauges above bound the MAP sizes; these bound the
    // BYTES held *inside* each call. A per-call Vec that grows per in-dialog
    // event on a long-held (OPTIONS-hold / re-INVITE) dialog and is pruned only
    // at terminal grows the heap with EVERY map count flat. These sums name it:
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

/// The label-keyed families of the counter set: a semi-open one's rows (one
/// per observed label set) or a fixed one's counts, every declared label set
/// at 0 from the start.
#[derive(Debug)]
struct Rows {
    // Inbound requests by method, outbound requests this worker originated
    // or relayed by method, inbound responses by CSeq method and status.
    requests: OpenRows,
    requests_out: OpenRows,
    responses: OpenRows,
    // Every repeat of a retained emission that left this worker — a rung of
    // a dialog-level ladder or a triggered re-send — by ladder, method and,
    // for a response, code. An original send is not a repeat.
    retransmits: OpenRows,
    // Dialog-level ladders that ran to their give-up, by the kind of
    // obligation left undischarged: the peer went deaf.
    repeat_give_ups: FixedCounts,
    // Model invariants broken and defects met, by name (ADR-0052 §8).
    model_defects: FixedCounts,
    // Outbound messages left unsent: their next hop is not an IP literal
    // (`router::unaddressable`), by kind.
    outbound_unaddressable: FixedCounts,
    // Messages and events that resolved to no call (`router::unroutable`),
    // in three classes that never overlap: wire messages that drew no
    // answer (by kind), wire requests answered on no call's behalf (by
    // method and code), this node's own events naming no call (by event and
    // method).
    unroutable_dropped: FixedCounts,
    unroutable_refused: OpenRows,
    unroutable_internal: OpenRows,
    // Quiet turns persisted without a bump or a flush, by kind.
    repl_quiet_turns: FixedCounts,
    // Inbound replication ops applied, by flow, peer and op: a reboot's bulk
    // reclaim is a sharp `recovery`/`create` step for that peer.
    repl_applied: OpenRows,
    // Catch-up/idle `Noop`s this node sent as a server, by flow and pulling
    // peer (ADR-0014): climbs on every healthy stream; a flat one names a
    // stuck serve loop.
    repl_noops_sent: OpenRows,
    // Forward flushes a backup refused as a regression of its own progress
    // (ADR-0031 D3), by the operation refused.
    repl_forward_flush_refused: FixedCounts,
    // Live calls resting at each state-machine cursor (ADR-0016),
    // a census replaced whole on the gauge cadence.
    sm_cursors: OpenRows,
}

impl Default for Rows {
    fn default() -> Self {
        use catalogue::worker as w;
        Self {
            requests: OpenRows::new(&w::REQUESTS),
            requests_out: OpenRows::new(&w::REQUESTS_OUT),
            responses: OpenRows::new(&w::RESPONSES),
            retransmits: OpenRows::new(&w::RETRANSMITS),
            repeat_give_ups: FixedCounts::new(&w::REPEAT_GIVE_UPS),
            model_defects: FixedCounts::new(&w::MODEL_DEFECTS),
            outbound_unaddressable: FixedCounts::new(&w::OUTBOUND_UNADDRESSABLE),
            unroutable_dropped: FixedCounts::new(&w::UNROUTABLE_DROPPED),
            unroutable_refused: OpenRows::new(&w::UNROUTABLE_REFUSED),
            unroutable_internal: OpenRows::new(&w::UNROUTABLE_INTERNAL),
            repl_quiet_turns: FixedCounts::new(&w::REPL_QUIET_TURNS),
            repl_applied: OpenRows::new(&w::REPL_APPLIED),
            repl_noops_sent: OpenRows::new(&w::REPL_NOOPS_SENT),
            repl_forward_flush_refused: FixedCounts::new(&w::REPL_FORWARD_FLUSH_REFUSED),
            sm_cursors: OpenRows::new(&w::SM_CURSORS),
        }
    }
}

/// Clone-cheap handle to the B2BUA counter set.
#[derive(Debug, Clone)]
pub struct B2buaMetrics {
    inner: Arc<Inner>,
    /// Cardinality-bounded per-peer failure/timeout counters
    /// (`b2bua_peer_failures_total{peer,scope,kind}`). Shared across clones.
    per_peer: Arc<crate::peer_failures::PeerFailures>,
    /// The router's new-call admission outcomes ([`crate::new_calls`]).
    new_calls: crate::new_calls::NewCallTally,
}

impl Default for B2buaMetrics {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
            per_peer: Arc::default(),
            new_calls: Default::default(),
        }
    }
}

/// One discard of each site, in label order: a release of any class counts
/// under `released`.
pub(crate) const DISCARD_SITES: [Discard; 4] = [
    Discard::QueueFull,
    Discard::AtCap,
    Discard::Released(RemovalClass::Terminated),
    Discard::Capped,
];

pub(crate) const fn discard_label(why: Discard) -> &'static str {
    match why {
        Discard::QueueFull => "queue_full",
        Discard::AtCap => "at_cap",
        Discard::Released(_) => "released",
        Discard::Capped => "capped",
    }
}

/// Every dispatcher bound, in label order.
pub(crate) const PAST_BOUNDS: [PastBound; 2] = [PastBound::Depth, PastBound::Cap];

pub(crate) const fn past_bound_label(bound: PastBound) -> &'static str {
    match bound {
        PastBound::Depth => "depth",
        PastBound::Cap => "cap",
    }
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

    /// The worker's limiter counters and gauges (`b2bua_limiter_*`).
    pub fn limiter(&self) -> &LimiterCounters {
        &self.inner.limiter
    }

    /// The router's new-call admission outcomes; `b2bua_new_calls_total`
    /// composes them with the other rungs' ([`crate::new_calls::NewCallCounts`]).
    pub fn new_calls(&self) -> &crate::new_calls::NewCallTally {
        &self.new_calls
    }

    /// Move the gauge of live calls near their lifetime cap by `delta`.
    pub fn add_calls_near_lifetime_cap(&self, delta: i64) {
        self.inner.calls_near_lifetime_cap.fetch_add(delta, Ordering::Relaxed);
    }

    /// Live calls offered more than 80 % of their lifetime cap (gauge).
    pub fn calls_near_lifetime_cap(&self) -> i64 {
        self.inner.calls_near_lifetime_cap.load(Ordering::Relaxed)
    }
    counter!(bump_overflow_refused, overflow_refused_total, overflow_refused);

    /// Move the gauge of items waiting in per-call overflows by `delta`.
    pub fn add_overflow_depth(&self, delta: i64) {
        self.inner.overflow_depth.fetch_add(delta, Ordering::Relaxed);
    }

    /// Items waiting in per-call overflows, all calls (gauge).
    pub fn overflow_depth(&self) -> i64 {
        self.inner.overflow_depth.load(Ordering::Relaxed)
    }
    counter!(bump_creation, creations_total, creations);
    counter!(bump_removal, removals_total, removals);

    /// One queue teardown of class `class`: bumps the removal and its class.
    pub fn bump_removal_of(&self, class: RemovalClass) {
        self.bump_removal();
        self.removal_class_counter(class).fetch_add(1, Ordering::Relaxed);
    }

    pub fn removals_of_total(&self, class: RemovalClass) -> u64 {
        self.removal_class_counter(class).load(Ordering::Relaxed)
    }

    /// One item queued past a dispatcher bound (see [`Room`](crate::dispatch::Room)).
    pub fn bump_past_bound(&self, bound: PastBound) {
        self.past_bound_counter(bound).fetch_add(1, Ordering::Relaxed);
    }

    pub fn past_bound_of_total(&self, bound: PastBound) -> u64 {
        self.past_bound_counter(bound).load(Ordering::Relaxed)
    }

    /// Items queued past either bound.
    pub fn past_bound_total(&self) -> u64 {
        self.past_bound_of_total(PastBound::Depth) + self.past_bound_of_total(PastBound::Cap)
    }

    fn past_bound_counter(&self, bound: PastBound) -> &AtomicU64 {
        match bound {
            PastBound::Depth => &self.inner.past_bound_depth,
            PastBound::Cap => &self.inner.past_bound_cap,
        }
    }

    /// One INVITE answered where the dispatcher discarded its body unrun,
    /// counted by site (every release class under `released`).
    pub fn bump_invite_discard_answered(&self, why: Discard) {
        self.invite_discard_counter(why).fetch_add(1, Ordering::Relaxed);
    }

    pub fn invite_discard_answered_of_total(&self, why: Discard) -> u64 {
        self.invite_discard_counter(why).load(Ordering::Relaxed)
    }

    /// INVITEs answered at any discard site.
    pub fn invite_discard_answered_total(&self) -> u64 {
        DISCARD_SITES.iter().map(|w| self.invite_discard_answered_of_total(*w)).sum()
    }

    fn invite_discard_counter(&self, why: Discard) -> &AtomicU64 {
        match why {
            Discard::QueueFull => &self.inner.invite_discard_answered_queue_full,
            Discard::AtCap => &self.inner.invite_discard_answered_at_cap,
            Discard::Released(_) => &self.inner.invite_discard_answered_released,
            Discard::Capped => &self.inner.invite_discard_answered_capped,
        }
    }

    fn removal_class_counter(&self, class: RemovalClass) -> &AtomicU64 {
        match class {
            RemovalClass::Terminated => &self.inner.removals_terminated,
            RemovalClass::SelfRelease => &self.inner.removals_self_release,
            RemovalClass::StandDown => &self.inner.removals_stand_down,
            RemovalClass::Orphan => &self.inner.removals_orphan,
        }
    }
    // Decision-application drop guard.
    // Second-final refusal (RFC 3261 §17.2.1) and the going-away gate.
    // Injectable store-fault seam (ADR-0023).
    // --- call reaper (ADR-0020) ---

    /// Count one catch-up/idle `Noop` SENT on a serve-side stream, for
    /// `b2bua_repl_noops_sent_total{flow,peer}` (ADR-0014). `flow` is the stream
    /// kind (`reclaim` = `Pri`, `backup` = `Bak`); `peer` is the pulling caller.
    /// Climbs continuously on a healthy stream (the idle interval, ADR-0045) — the
    /// backup-holder's "I have sent you everything in this flow" liveness sign.
    pub fn record_repl_noop_sent(&self, flow: &str, peer: &str) {
        self.inner.rows.repl_noops_sent.add(&[flow, peer], 1);
    }

    /// Record one completed drain: its reason label into
    /// `b2bua_drain_exits_total{reason}`, its duration into the
    /// `b2bua_drain_seconds` histogram (ADR-0031 D2), and its release flush
    /// into `b2bua_limiter_release_flushes_total{outcome}` and
    /// `b2bua_limiter_release_flush_seconds_total` (ADR-0040 decision 9).
    pub fn record_drain_exit(&self, outcome: &crate::drain::DrainOutcome) {
        let elapsed = outcome.elapsed;
        cells::add_at(&self.inner.drain_exits, outcome.exit as usize, 1);
        self.inner.limiter.count_release_flush(&outcome.release_flush);
        let secs = elapsed.as_secs_f64();
        let buckets = DRAIN_BUCKETS.iter().zip(&self.inner.drain_seconds_buckets);
        buckets.filter(|(le, _)| secs <= **le).for_each(|(_, n)| cells::add(n, 1));
        cells::add(&self.inner.drain_seconds_sum_ms, sip_clock::wall::millis_u64(elapsed));
        self.inner.drain_seconds_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Drains that returned for `reason` (test/observability).
    pub fn drain_exits(&self, reason: &str) -> u64 {
        DrainExit::ALL
            .iter()
            .find(|e| e.label() == reason)
            .map_or(0, |e| cells::get_at(&self.inner.drain_exits, *e as usize))
    }

    /// Count one forward flush the Backup flow refused because it would regress
    /// the backup's own progress (ADR-0031 D3). `op` is the refused operation:
    /// `put` (a body that branches off the Element, or a `b'` behind the
    /// Element's `b`) or `delete` (an `Active` Element whose caller was answered
    /// and whose answer the authority never took).
    pub fn record_repl_forward_flush_refused(&self, op: &str) {
        self.inner.rows.repl_forward_flush_refused.add(&[op], 1);
    }

    /// Forward flushes refused for `op` (test/observability).
    pub fn repl_forward_flush_refused(&self, op: &str) -> u64 {
        self.inner.rows.repl_forward_flush_refused.get(&[op])
    }

    /// Count one inbound request by SIP method, for `b2bua_requests_total{method}`.
    pub fn record_request(&self, method: &str) {
        self.inner.rows.requests.add(&[&method.to_ascii_uppercase()], 1);
    }

    /// Count one OUTBOUND request this worker originated/relayed, for
    /// `b2bua_requests_out_total{method}`. The in-dialog keepalive OPTIONS lands
    /// here; pairing OPTIONS-out with the inbound `responses_total{OPTIONS,200}`
    /// isolates the keepalive round-trip (sent vs answered) on the b2bua itself.
    pub fn record_request_out(&self, method: &str) {
        self.inner.rows.requests_out.add(&[&method.to_ascii_uppercase()], 1);
    }

    /// Count one inbound response by its CSeq method + status code, for
    /// `b2bua_responses_total{method,code}`.
    pub fn record_response(&self, method: &str, code: u16) {
        let (method, code) = (method.to_ascii_uppercase(), code.to_string());
        self.inner.rows.responses.add(&[&method, &code], 1);
    }

    /// Count one repeat that left this worker, for
    /// `b2bua_retransmits_total{ladder,method,code}`: `ladder` is the
    /// `sip_retransmit::Class` name that paced it or `trigger`, `method` the
    /// CSeq method of the repeated message, `code` its status for a response.
    pub fn record_retransmit(&self, ladder: &str, method: &str, code: Option<u16>) {
        self.inner.rows.retransmits.add(&retransmit_row(ladder, method, code).as_refs(), 1);
    }

    /// The count of one `{ladder,method,code}` row.
    pub fn retransmits_total(&self, ladder: &str, method: &str, code: Option<u16>) -> u64 {
        self.inner.rows.retransmits.get(&retransmit_row(ladder, method, code).as_refs())
    }

    /// Record one per-peer failure of `kind` against `peer` in `scope`
    /// (`b2bua_peer_failures_total{peer,scope,kind}`; cardinality-bounded, see
    /// [`crate::peer_failures::PeerFailures`]).
    pub fn record_peer_failure(
        &self,
        peer: &std::net::SocketAddr,
        scope: crate::peer_failures::PeerScope,
        kind: crate::peer_failures::PeerFailureKind,
    ) {
        self.per_peer.record(peer, scope, kind);
    }

    // --- replication ---

    /// Count one quiet turn of `kind`, for `b2bua_repl_quiet_turns_total{kind}`.
    pub fn record_quiet_turn(&self, kind: &str) {
        self.inner.rows.repl_quiet_turns.add(&[kind], 1);
    }

    /// The quiet turns of one kind.
    pub fn repl_quiet_turns_total(&self, kind: &str) -> u64 {
        self.inner.rows.repl_quiet_turns.get(&[kind])
    }

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
        self.inner.rows.repl_applied.add(&[flow, peer, op], 1);
    }
    /// Sum of all applied replication ops, every flow, peer and op
    /// (test/observability convenience).
    pub fn repl_applied_sum(&self) -> u64 {
        self.inner.rows.repl_applied.sum()
    }
    /// Backup replicas this node currently holds, derived from the `backup`-flow
    /// op counts (creates − deletes). Test/observability convenience;
    /// production reads the sampled `repl_meta_backup` (this derivation does
    /// not see TTL eviction — fine for the unit tests that never evict).
    pub fn repl_backup_replicas(&self) -> u64 {
        let rows = self.inner.rows.repl_applied.rows();
        let get = |op: &str| {
            rows.iter()
                .filter(|(k, _)| {
                    k.first().is_some_and(|f| f == "backup") && k.get(2).is_some_and(|o| o == op)
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

    /// Publish the sampled clock-skew divergence (`raw_wall − now_ms`, signed ms),
    /// stored as the i64's bit pattern (no signed atomic) for `clock_wall_divergence_ms`
    /// and read back by [`clock_wall_divergence_ms`](Self::clock_wall_divergence_ms).
    pub fn set_clock_wall_divergence_ms(&self, divergence_ms: i64) {
        self.inner.clock_wall_divergence_ms.store(divergence_ms.cast_unsigned(), Ordering::Relaxed);
    }
    pub fn clock_wall_divergence_ms(&self) -> i64 {
        self.inner.clock_wall_divergence_ms.load(Ordering::Relaxed).cast_signed()
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

    /// Set whether this worker is withdrawn from routing while still running
    /// (ADR-0031 D6). Written by the replication supervisor on the observation.
    pub fn set_withdrawn_running(&self, withdrawn: bool) {
        self.inner.withdrawn_running.store(u64::from(withdrawn), Ordering::Relaxed);
    }
    pub fn withdrawn_running(&self) -> bool {
        self.inner.withdrawn_running.load(Ordering::Relaxed) == 1
    }

    /// Set the number of replication peers currently pulled while not `ready`
    /// (present in membership only, ADR-0031 D1). Written by the supervisor at
    /// every reconcile.
    pub fn set_repl_peers_pulled_not_ready(&self, n: u64) {
        self.inner.repl_peers_pulled_not_ready.store(n, Ordering::Relaxed);
    }
    pub fn repl_peers_pulled_not_ready(&self) -> u64 {
        self.inner.repl_peers_pulled_not_ready.load(Ordering::Relaxed)
    }

    /// Replace the state-machine cursor census (ADR-0016) wholesale —
    /// `census` maps `(machine, state)` to the count of live calls resting there,
    /// sampled from the call map under the store lock on the slow gauge cadence.
    /// Overwriting (rather than incrementing) means a cursor that drained to zero
    /// disappears from the next scrape instead of sticking at its last value.
    pub fn set_sm_cursor_census(&self, census: BTreeMap<(String, String), u64>) {
        let rows =
            census.iter().map(|((machine, state), n)| (vec![machine.as_str(), state.as_str()], *n));
        self.inner.rows.sm_cursors.replace(rows);
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
        catalogue::plain::render(self, &mut s);
        catalogue::worker::CALL_CREATIONS.render_value(&mut s, creations);
        catalogue::worker::CALL_REMOVALS.render_value(&mut s, removals);
        shell::render(&mut s);

        // Per-method SIP traffic, the repeats and give-ups, replication per
        // stream, the drains.
        let i = &self.inner;
        let rows = &i.rows;
        rows.requests.render(&mut s);
        rows.responses.render(&mut s);
        rows.requests_out.render(&mut s);
        rows.retransmits.render(&mut s);
        rows.outbound_unaddressable.render(&mut s);
        rows.unroutable_dropped.render(&mut s);
        rows.unroutable_refused.render(&mut s);
        rows.unroutable_internal.render(&mut s);
        rows.repeat_give_ups.render(&mut s);
        rows.model_defects.render(&mut s);
        rows.repl_quiet_turns.render(&mut s);
        rows.repl_applied.render(&mut s);
        rows.repl_noops_sent.render(&mut s);
        rows.repl_forward_flush_refused.render(&mut s);
        catalogue::worker::DRAIN_EXITS.render(&mut s, |series| {
            cells::get_at(&i.drain_exits, series.index(&catalogue::worker::DRAIN_REASON))
        });
        catalogue::worker::DRAIN_SECONDS.render_histogram(&mut s, |_| HistogramValue {
            buckets: catalogue::worker::DRAIN_BUCKETS
                .iter()
                .zip(&i.drain_seconds_buckets)
                .map(|(le, n)| (*le, n.load(Ordering::Relaxed)))
                .collect(),
            sum: i.drain_seconds_sum_ms.load(Ordering::Relaxed) as f64 / 1_000.0,
            count: i.drain_seconds_count.load(Ordering::Relaxed),
        });
        operator::render(self, &mut s);
        self.inner.limiter.render(&mut s);
        catalogue::worker::CALL_REMOVALS_BY_CLASS.render(&mut s, |r| {
            cells::total(&RemovalClass::ALL, r.index(&REMOVAL_CLASS), |c| self.removals_of_total(c))
        });
        catalogue::worker::DISPATCH_PAST_BOUND.render(&mut s, |series| {
            cells::total(&PAST_BOUNDS, series.index(&PAST_BOUND), |b| self.past_bound_of_total(b))
        });
        catalogue::worker::DISPATCH_OVERFLOW_REFUSED
            .render_value(&mut s, self.overflow_refused_total());
        catalogue::worker::DISPATCH_OVERFLOW_DEPTH.render_value(&mut s, self.overflow_depth());
        catalogue::worker::CALLS_NEAR_LIFETIME_CAP
            .render_value(&mut s, self.calls_near_lifetime_cap());
        catalogue::DISPATCH_INVITE_DISCARD_ANSWERED.render(&mut s, |site| {
            let at = site.index(&catalogue::SITE);
            cells::total(&DISCARD_SITES, at, |d| self.invite_discard_answered_of_total(d))
        });

        // Gauges last.
        catalogue::worker::ACTIVE_CALLS.render_value(&mut s, active);
        catalogue::worker::TIMER_QUEUE_LEN.render_value(&mut s, self.timer_queue_len());
        catalogue::worker::TIMER_LIVE.render_value(&mut s, self.timer_live());
        catalogue::worker::CLOCK_WALL_DIVERGENCE_MS
            .render_value(&mut s, self.clock_wall_divergence_ms());
        // Memory-attribution gauges: per-map sizes so a RSS climb can be pinned
        // to a specific map even when active_calls is flat. b2bua_store_calls is
        // the TRUE live call-map length — a gap vs b2bua_active_calls localises a
        // store-side leak; a sibling map (sip_index/indexed/locks/takeover_at)
        // outgrowing it names which one.
        catalogue::worker::STORE_CALLS
            .render_value(&mut s, self.inner.store_calls.load(Ordering::Relaxed));
        catalogue::worker::STORE_SIP_INDEX
            .render_value(&mut s, self.inner.store_sip_index.load(Ordering::Relaxed));
        catalogue::worker::STORE_INDEXED
            .render_value(&mut s, self.inner.store_indexed.load(Ordering::Relaxed));
        catalogue::worker::STORE_LOCKS
            .render_value(&mut s, self.inner.store_locks.load(Ordering::Relaxed));
        catalogue::worker::STORE_TAKEOVER_AT
            .render_value(&mut s, self.inner.store_takeover_at.load(Ordering::Relaxed));
        catalogue::worker::STORE_TOUCHED
            .render_value(&mut s, self.inner.store_touched.load(Ordering::Relaxed));
        catalogue::worker::STORE_BODIES
            .render_value(&mut s, self.inner.store_bodies.load(Ordering::Relaxed));
        catalogue::worker::STORE_IDX_ENTRIES
            .render_value(&mut s, self.inner.store_idx_entries.load(Ordering::Relaxed));
        catalogue::worker::STORE_TOMBSTONES
            .render_value(&mut s, self.inner.store_tombstones.load(Ordering::Relaxed));
        catalogue::worker::REPL_META
            .render_value(&mut s, self.inner.repl_meta_total.load(Ordering::Relaxed));
        catalogue::worker::REPL_META_BACKUP
            .render_value(&mut s, self.inner.repl_meta_backup.load(Ordering::Relaxed));
        catalogue::worker::REPL_CHANGELOG_ENTRIES
            .render_value(&mut s, self.inner.repl_changelog_entries.load(Ordering::Relaxed));
        catalogue::worker::REPL_CHANGELOG_PEERS
            .render_value(&mut s, self.inner.repl_changelog_peers.load(Ordering::Relaxed));
        catalogue::worker::WITHDRAWN_RUNNING
            .render_value(&mut s, self.inner.withdrawn_running.load(Ordering::Relaxed));
        catalogue::worker::REPL_PEERS_PULLED_NOT_READY
            .render_value(&mut s, self.repl_peers_pulled_not_ready());
        catalogue::worker::REPL_BOOTSTRAP_LAST_APPLIED
            .render_value(&mut s, self.repl_bootstrap_last_applied());
        catalogue::worker::REPL_RECLAIM_SCANNED.render_value(&mut s, self.repl_reclaim_scanned());
        catalogue::worker::REPL_RECLAIM_MATERIALIZED
            .render_value(&mut s, self.repl_reclaim_materialized());
        // Per-call Vec census: bytes-inside-each-call. The sum whose ratio over
        // store_calls climbs while every count-gauge is flat names the leaking
        // per-call Vec (a held dialog's per-event tail never pruned till terminal).
        catalogue::worker::CENSUS_CDR_EVENTS
            .render_value(&mut s, self.inner.census_cdr_events.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_PENDING_REQUESTS
            .render_value(&mut s, self.inner.census_pending_requests.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_PENDING_REQUESTS_MAX
            .render_value(&mut s, self.inner.census_pending_requests_max.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_DIALOGS
            .render_value(&mut s, self.inner.census_dialogs.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_ROUTE_SET
            .render_value(&mut s, self.inner.census_route_set.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_TIMERS
            .render_value(&mut s, self.inner.census_timers.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_TAG_MAP
            .render_value(&mut s, self.inner.census_tag_map.load(Ordering::Relaxed));
        catalogue::worker::CENSUS_B_LEGS
            .render_value(&mut s, self.inner.census_b_legs.load(Ordering::Relaxed));
        // State-machine cursor census (ADR-0016): live calls per
        // (machine,state), a census replaced whole, so a drained cursor drops
        // out; then the per-peer failures.
        rows.sm_cursors.render(&mut s);
        self.per_peer.render(&mut s);
        s
    }
}

#[cfg(test)]
mod tests;
