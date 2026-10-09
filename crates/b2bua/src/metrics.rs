//! B2BUA metrics — atomic counters/gauges (the source's `MetricsRegistry`
//! surface reduced to the counters the ported paths move). Cheap to clone
//! (one `Arc`); read with the `*_total` accessors.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use metric_catalogue::{FixedCounts, HistogramValue, OpenRows};

use crate::dispatch::{Discard, PastBound};
use crate::drain::DrainExit;
use crate::ingress_brake::IngressBrakeCounters;
use catalogue::worker::DRAIN_BUCKETS;

pub mod catalogue;
mod limiter;

pub use limiter::{
    AdmitSite, LimiterCounters, LimiterFailure, LimiterOp, LimiterTask, RefreshDiscard,
    RefreshGiveUp, ReleaseGiveUp,
};

/// A `b2bua_retransmits_total` row: method uppercased, the code's digits for
/// a response.
struct RetransmitRow {
    ladder: String,
    method: String,
    code: Option<String>,
}

impl RetransmitRow {
    fn as_refs(&self) -> Vec<&str> {
        let mut row = vec![self.ladder.as_str(), self.method.as_str()];
        row.extend(self.code.as_deref());
        row
    }
}

fn retransmit_row(ladder: &str, method: &str, code: Option<u16>) -> RetransmitRow {
    RetransmitRow {
        ladder: ladder.to_owned(),
        method: method.to_ascii_uppercase(),
        code: code.map(|c| c.to_string()),
    }
}

#[derive(Debug, Default)]
struct Inner {
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
    drain_seconds_buckets: [AtomicU64; DRAIN_BUCKETS.len()],
    drain_seconds_sum_ms: AtomicU64,
    drain_seconds_count: AtomicU64,
    // The worker's view of its call limiter (ADR-0040).
    limiter: LimiterCounters,
    // dispatcher
    queue_drops: AtomicU64,
    cap_drops: AtomicU64,
    release_discards: AtomicU64,
    past_bound_depth: AtomicU64,
    past_bound_cap: AtomicU64,
    overflow_refused: AtomicU64,
    overflow_depth: std::sync::atomic::AtomicI64,
    invite_discard_answered_queue_full: AtomicU64,
    invite_discard_answered_at_cap: AtomicU64,
    invite_discard_answered_released: AtomicU64,
    invite_discard_answered_capped: AtomicU64,
    capped_refusals: AtomicU64,
    capped_request_answered: AtomicU64,
    calls_near_lifetime_cap: std::sync::atomic::AtomicI64,
    saturation: AtomicU64,
    new_call_share_waits: AtomicU64,
    // MAX_MESSAGES_PER_CALL cap-defense: calls torn down for crossing the
    // per-call message cap (a runaway re-INVITE/OPTIONS storm or glare loop),
    // so no call processes unbounded in-dialog events.
    message_cap_terminated: AtomicU64,
    // Calls that crossed their lifetime message cap (the work bound).
    message_cap_lifetime_crossed: AtomicU64,
    creations: AtomicU64,
    removals: AtomicU64,
    removals_terminated: AtomicU64,
    removals_self_release: AtomicU64,
    removals_orphan: AtomicU64,
    // router / handler
    handler_timeouts: AtomicU64,
    force_purge: AtomicU64,
    fast_reject_terminating: AtomicU64,
    // call reaper (ADR-0020). `handler_panics` counts dispatcher-observed body
    // panics (pre-reaper these were swallowed — the zero-CDR leak class);
    // `reaper_verdicts` counts injected synthetic events (stale + fatal +
    // discharge); `reaper_discharged` is the ALARM — the rules path itself
    // failed twice for a call; expected ~0 in any healthy run.
    handler_panics: AtomicU64,
    reaper_sweeps: AtomicU64,
    reaper_sweep_panics: AtomicU64,
    replica_reap_panics: AtomicU64,
    reaper_verdicts: AtomicU64,
    reaper_discharged: AtomicU64,
    // cdr
    cdr_written: AtomicU64,
    cdr_dropped: AtomicU64,
    // Decision-application drop guard: a `/call/new` decision result (route
    // or reject) that landed on a call whose initial INVITE the caller already
    // CANCELed — dropped whole: no b-leg launch, no second final on the a-leg's
    // completed transaction. A non-zero rate measures the caller-gives-up-
    // during-routing race, not a fault.
    decision_dropped_cancelled: AtomicU64,
    // Second-final refusal (RFC 3261 §17.2.1): a final the call layer authored
    // toward the a-leg's initial INVITE while that transaction already carried
    // one — refused at the a-leg response seam, never built. A rule made
    // progress on a call already answered or going away; expected 0 in any
    // healthy run.
    second_final_refused: AtomicU64,
    // Late-provisional refusal (RFC 3261 §13.3.1.1 / §17.2.1): a provisional
    // the call layer authored toward the a-leg's initial INVITE while that
    // transaction already sent its final — refused at the a-leg response
    // seam, never built. A rule showed a ringing leg to an answered caller;
    // expected 0 in any healthy run.
    provisional_after_final_refused: AtomicU64,
    // Calls reaching Terminated with no termination record (expected 0).
    termination_unrecorded: AtomicU64,
    // Going-away gate: an asynchronous trigger (timer fire, transaction
    // timeout, internal-event fold) that landed on a Terminating/Terminated
    // call and matched a rule that is not a teardown rule — absorbed before
    // it ran. Measures the race between a call's own clocks and its teardown,
    // not a fault; a rule named here on a live call would have made progress.
    going_away_absorbed: AtomicU64,
    // PRACKs answered 200 after their call's release (RFC 3262 §3).
    late_prack_answered: AtomicU64,
    // Another incarnation's event: a timer fire, transaction timeout,
    // callout result or late message of an earlier call on the same
    // callRef, dropped before the live call's rules read it. Measures the
    // race between a call's end and a retry born on its callRef, not a fault.
    other_incarnation_dropped: AtomicU64,
    // Injectable store-fault seam (ADR-0023): `store_fault_rejected` = live
    // lookups that failed CLOSED with a 500 final (initial-INVITE dialog-
    // existence check or in-dialog request fetch); `store_fault_audit_skipped`
    // = keepalive/audit cycles skipped FAIL-OPEN (call kept up, timer re-armed
    // so liveness detection resumes next interval — a store fault alone never
    // tears down an established call). Both stay 0 unless a `StoreFaults`
    // handle is armed (tests) or a future fallible store is wired.
    store_fault_rejected: AtomicU64,
    store_fault_audit_skipped: AtomicU64,
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
    // replication (peer-to-peer HA; separate namespace `b2bua_repl_*`). These
    // localise an HA failure to a layer: `flush_propagated` rising on the PRIMARY
    // proves it is attempting to replicate (the proxy cookie stamped
    // `topology.bak`); the per-stream `applied` breakdown (below) proves the
    // replica actually arrived; `takeover_resolved`/`hydrated` prove a failed-over
    // in-dialog request found + loaded the replica on the backup. The TRUE resident
    // backup count is the sampled `repl_meta_backup` gauge (not a counter-derived
    // estimate).
    repl_flush_propagated: AtomicU64,
    repl_takeover_resolved: AtomicU64,
    repl_takeover_hydrated: AtomicU64,
    // Refusals of a `Terminated` replica; the rule is `router::materialise`.
    repl_takeover_refused_terminated: AtomicU64,
    // Reverse flushes a live primary refused to fold; the gate is `router::reclaim`.
    repl_reverse_flush_refused: AtomicU64,
    // Fail-back (ADR-0011 X11 / ADR-0014): `reclaimed` = calls a rebooted primary
    // re-materialised into its live map (active reclaim); `self_release` = acting-
    // backup takeover copies the backup *self-released* once the transaction(s) it
    // served reached a terminal state (ADR-0014). After a kill_worker+reclaim,
    // `self_release` ≈ takeover copies shed and the active/sipp gap reaps to ~0.
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
    repl_bootstrap_seeded: AtomicU64,
    repl_bootstrap_stalled: AtomicU64,
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
    // climbing while store_bodies is flat is THAT leak (the no-chaos RSS climb).
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

/// Why a per-call dispatch queue was torn down: the release that poisoned it.
/// Every removal has exactly one class; they partition `b2bua_call_removals_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalClass {
    /// A call that terminated: it owes its CDR.
    Terminated,
    /// A takeover copy shed by an acting backup (ADR-0014): the call lives on
    /// at its primary, which owes the CDR.
    SelfRelease,
    /// A queue that never held a call: an initial INVITE shed statelessly
    /// (overload, store fault) or an event naming no resident call. No CDR.
    Orphan,
}

impl RemovalClass {
    /// Every class, in declaration order.
    pub const ALL: [RemovalClass; 3] =
        [RemovalClass::Terminated, RemovalClass::SelfRelease, RemovalClass::Orphan];

    pub const fn label(self) -> &'static str {
        match self {
            RemovalClass::Terminated => "terminated",
            RemovalClass::SelfRelease => "self_release",
            RemovalClass::Orphan => "orphan",
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

    counter!(bump_message_cap_terminated, message_cap_terminated_total, message_cap_terminated);
    counter!(
        bump_message_cap_lifetime_crossed,
        message_cap_lifetime_crossed_total,
        message_cap_lifetime_crossed
    );
    counter!(bump_capped_refusal, capped_refusals_total, capped_refusals);
    counter!(bump_capped_request_answered, capped_request_answered_total, capped_request_answered);

    /// Move the gauge of live calls near their lifetime cap by `delta`.
    pub fn add_calls_near_lifetime_cap(&self, delta: i64) {
        self.inner.calls_near_lifetime_cap.fetch_add(delta, Ordering::Relaxed);
    }

    /// Live calls offered more than 80 % of their lifetime cap (gauge).
    pub fn calls_near_lifetime_cap(&self) -> i64 {
        self.inner.calls_near_lifetime_cap.load(Ordering::Relaxed)
    }
    counter!(bump_queue_drop, queue_drops_total, queue_drops);
    counter!(bump_cap_drop, cap_drops_total, cap_drops);
    counter!(bump_release_discard, release_discards_total, release_discards);
    counter!(bump_overflow_refused, overflow_refused_total, overflow_refused);

    /// Move the gauge of items waiting in per-call overflows by `delta`.
    pub fn add_overflow_depth(&self, delta: i64) {
        self.inner.overflow_depth.fetch_add(delta, Ordering::Relaxed);
    }

    /// Items waiting in per-call overflows, all calls (gauge).
    pub fn overflow_depth(&self) -> i64 {
        self.inner.overflow_depth.load(Ordering::Relaxed)
    }
    counter!(bump_saturation, saturation_total, saturation);
    counter!(bump_new_call_share_wait, new_call_share_waits_total, new_call_share_waits);
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
            RemovalClass::Orphan => &self.inner.removals_orphan,
        }
    }
    counter!(bump_handler_timeout, handler_timeouts_total, handler_timeouts);
    counter!(bump_force_purge, force_purge_total, force_purge);
    counter!(bump_fast_reject_terminating, fast_reject_terminating_total, fast_reject_terminating);
    counter!(bump_cdr_written, cdr_written_total, cdr_written);
    counter!(bump_cdr_dropped, cdr_dropped_total, cdr_dropped);
    // Decision-application drop guard.
    counter!(
        bump_decision_dropped_cancelled,
        decision_dropped_cancelled_total,
        decision_dropped_cancelled
    );
    // Second-final refusal (RFC 3261 §17.2.1) and the going-away gate.
    counter!(bump_second_final_refused, second_final_refused_total, second_final_refused);
    counter!(
        bump_provisional_after_final_refused,
        provisional_after_final_refused_total,
        provisional_after_final_refused
    );
    counter!(bump_termination_unrecorded, termination_unrecorded_total, termination_unrecorded);
    counter!(bump_going_away_absorbed, going_away_absorbed_total, going_away_absorbed);
    counter!(bump_late_prack_answered, late_prack_answered_total, late_prack_answered);
    counter!(
        bump_other_incarnation_dropped,
        other_incarnation_dropped_total,
        other_incarnation_dropped
    );
    // Injectable store-fault seam (ADR-0023).
    counter!(bump_store_fault_rejected, store_fault_rejected_total, store_fault_rejected);
    counter!(
        bump_store_fault_audit_skipped,
        store_fault_audit_skipped_total,
        store_fault_audit_skipped
    );
    // --- call reaper (ADR-0020) ---
    counter!(bump_handler_panic, handler_panics_total, handler_panics);
    counter!(bump_reaper_sweep, reaper_sweeps_total, reaper_sweeps);
    counter!(bump_reaper_sweep_panic, reaper_sweep_panics_total, reaper_sweep_panics);
    counter!(bump_replica_reap_panic, replica_reap_panics_total, replica_reap_panics);
    counter!(bump_reaper_verdict, reaper_verdicts_total, reaper_verdicts);
    counter!(bump_reaper_discharged, reaper_discharged_total, reaper_discharged);

    /// Count one catch-up/idle `Noop` SENT on a serve-side stream, for
    /// `b2bua_repl_noops_sent_total{flow,peer}` (ADR-0014). `flow` is the stream
    /// kind (`reclaim` = `Pri`, `backup` = `Bak`); `peer` is the pulling caller.
    /// Climbs continuously on a healthy stream (the ~20s idle floor) — the
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
        self.inner.drain_exits[outcome.exit as usize].fetch_add(1, Ordering::Relaxed);
        self.inner.limiter.count_release_flush(&outcome.release_flush);
        let secs = elapsed.as_secs_f64();
        for (i, le) in DRAIN_BUCKETS.iter().enumerate() {
            if secs <= *le {
                self.inner.drain_seconds_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.inner.drain_seconds_sum_ms.fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
        self.inner.drain_seconds_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Drains that returned for `reason` (test/observability).
    pub fn drain_exits(&self, reason: &str) -> u64 {
        DrainExit::ALL
            .iter()
            .find(|e| e.label() == reason)
            .map_or(0, |e| self.inner.drain_exits[*e as usize].load(Ordering::Relaxed))
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

    /// Count one wire message naming no call that drew no answer, for
    /// `b2bua_unroutable_dropped_total{kind}`: `kind` is `ACK` or a response's
    /// status class (`1xx`…`6xx`).
    pub fn record_unroutable_dropped(&self, kind: &str) {
        self.inner.rows.unroutable_dropped.add(&[kind], 1);
    }

    /// Wire messages naming no call that drew no answer, every kind.
    pub fn unroutable_dropped_total(&self) -> u64 {
        self.inner.rows.unroutable_dropped.sum()
    }

    /// Wire messages naming no call that drew no answer, of one `kind`.
    pub fn unroutable_dropped_of(&self, kind: &str) -> u64 {
        self.inner.rows.unroutable_dropped.get(&[kind])
    }

    /// Count one wire request naming no call that this node answered `code`,
    /// for `b2bua_unroutable_refused_total{method,code}`.
    pub fn record_unroutable_refused(&self, method: &str, code: u16) {
        self.inner.rows.unroutable_refused.add(&[method, &code.to_string()], 1);
    }

    /// Wire requests naming no call answered `code`, of one `method`.
    pub fn unroutable_refused_of(&self, method: &str, code: u16) -> u64 {
        self.inner.rows.unroutable_refused.get(&[method, &code.to_string()])
    }

    /// Wire requests naming no call that this node answered, every kind.
    pub fn unroutable_refused_total(&self) -> u64 {
        self.inner.rows.unroutable_refused.sum()
    }

    /// Count one of this node's own events that named no call, for
    /// `b2bua_unroutable_internal_total{event,method}`; `method` is the timed-out
    /// request's, empty for an event that carries none.
    pub fn record_unroutable_internal(&self, event: &str, method: &str) {
        self.inner.rows.unroutable_internal.add(&[event, method], 1);
    }

    /// This node's own events that named no call, of one `event` and `method`.
    pub fn unroutable_internal_of(&self, event: &str, method: &str) -> u64 {
        self.inner.rows.unroutable_internal.get(&[event, method])
    }

    /// This node's own events that named no call, every kind.
    pub fn unroutable_internal_total(&self) -> u64 {
        self.inner.rows.unroutable_internal.sum()
    }

    /// Count one dialog-level ladder that ran to its give-up, for
    /// `b2bua_repeat_give_ups_total{obligation}`.
    pub fn record_repeat_give_up(&self, obligation: &str) {
        self.inner.rows.repeat_give_ups.add(&[obligation], 1);
    }

    /// The give-ups of one obligation kind.
    pub fn repeat_give_ups_total(&self, obligation: &str) -> u64 {
        self.inner.rows.repeat_give_ups.get(&[obligation])
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
    counter!(bump_repl_flush_propagated, repl_flush_propagated_total, repl_flush_propagated);

    /// Count one quiet turn of `kind`, for `b2bua_repl_quiet_turns_total{kind}`.
    pub fn record_quiet_turn(&self, kind: &str) {
        self.inner.rows.repl_quiet_turns.add(&[kind], 1);
    }

    /// The quiet turns of one kind.
    pub fn repl_quiet_turns_total(&self, kind: &str) -> u64 {
        self.inner.rows.repl_quiet_turns.get(&[kind])
    }
    counter!(bump_repl_takeover_resolved, repl_takeover_resolved_total, repl_takeover_resolved);
    counter!(bump_repl_takeover_hydrated, repl_takeover_hydrated_total, repl_takeover_hydrated);
    counter!(
        bump_repl_reverse_flush_refused,
        repl_reverse_flush_refused_total,
        repl_reverse_flush_refused
    );
    counter!(
        bump_repl_takeover_refused_terminated,
        repl_takeover_refused_terminated_total,
        repl_takeover_refused_terminated
    );
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
                .filter(|(k, _)| k[0] == "backup" && k[2] == op)
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

    /// Publish the sampled clock-skew divergence (`raw_wall − now_ms`, signed ms)
    /// for `clock_wall_divergence_ms`. Stored as the i64's bit pattern (no signed
    /// atomic); [`clock_wall_divergence_ms`](Self::clock_wall_divergence_ms) reads
    /// it back.
    pub fn set_clock_wall_divergence_ms(&self, divergence_ms: i64) {
        self.inner.clock_wall_divergence_ms.store(divergence_ms as u64, Ordering::Relaxed);
    }
    pub fn clock_wall_divergence_ms(&self) -> i64 {
        self.inner.clock_wall_divergence_ms.load(Ordering::Relaxed) as i64
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
        catalogue::worker::MESSAGE_CAP_LIFETIME_CROSSED
            .render_value(&mut s, self.message_cap_lifetime_crossed_total());
        catalogue::DISPATCH_CAPPED_REFUSALS.render_value(&mut s, self.capped_refusals_total());
        catalogue::DISPATCH_CAPPED_REQUEST_ANSWERED
            .render_value(&mut s, self.capped_request_answered_total());
        catalogue::worker::MESSAGE_CAP_TERMINATED
            .render_value(&mut s, self.message_cap_terminated_total());
        catalogue::DISPATCH_QUEUE_DROPS.render_value(&mut s, self.queue_drops_total());
        catalogue::DISPATCH_CAP_DROPS.render_value(&mut s, self.cap_drops_total());
        catalogue::DISPATCH_RELEASE_DISCARDS.render_value(&mut s, self.release_discards_total());
        catalogue::DISPATCH_SATURATION.render_value(&mut s, self.saturation_total());
        catalogue::DISPATCH_NEW_CALL_SHARE_WAITS
            .render_value(&mut s, self.new_call_share_waits_total());
        catalogue::worker::CALL_CREATIONS.render_value(&mut s, creations);
        catalogue::worker::CALL_REMOVALS.render_value(&mut s, removals);
        catalogue::worker::HANDLER_TIMEOUTS.render_value(&mut s, self.handler_timeouts_total());
        catalogue::worker::FORCE_PURGE.render_value(&mut s, self.force_purge_total());
        catalogue::worker::FAST_REJECT_TERMINATING
            .render_value(&mut s, self.fast_reject_terminating_total());
        catalogue::worker::CDR_WRITTEN.render_value(&mut s, self.cdr_written_total());
        catalogue::worker::CDR_DROPPED.render_value(&mut s, self.cdr_dropped_total());
        // ── decision-application drop guard ──
        catalogue::worker::DECISION_DROPPED_CANCELLED
            .render_value(&mut s, self.decision_dropped_cancelled_total());
        // ── a call already going away authors no further progress ──
        catalogue::worker::TERMINATION_UNRECORDED
            .render_value(&mut s, self.termination_unrecorded_total());
        catalogue::worker::SECOND_FINAL_REFUSED
            .render_value(&mut s, self.second_final_refused_total());
        catalogue::worker::PROVISIONAL_AFTER_FINAL_REFUSED
            .render_value(&mut s, self.provisional_after_final_refused_total());
        catalogue::worker::GOING_AWAY_ABSORBED
            .render_value(&mut s, self.going_away_absorbed_total());
        catalogue::worker::LATE_PRACK_ANSWERED
            .render_value(&mut s, self.late_prack_answered_total());
        catalogue::worker::OTHER_INCARNATION_DROPPED
            .render_value(&mut s, self.other_incarnation_dropped_total());
        // ── injectable store-fault seam (ADR-0023) ──
        catalogue::worker::STORE_FAULT_REJECTED
            .render_value(&mut s, self.store_fault_rejected_total());
        catalogue::worker::STORE_FAULT_AUDIT_SKIPPED
            .render_value(&mut s, self.store_fault_audit_skipped_total());
        // ── call reaper (ADR-0020) ──
        catalogue::worker::HANDLER_PANICS.render_value(&mut s, self.handler_panics_total());
        catalogue::worker::REAPER_SWEEPS.render_value(&mut s, self.reaper_sweeps_total());
        catalogue::worker::REAPER_SWEEP_PANICS
            .render_value(&mut s, self.reaper_sweep_panics_total());
        catalogue::worker::REPLICA_REAP_PANICS
            .render_value(&mut s, self.replica_reap_panics_total());
        catalogue::worker::STORE_POISONED_LOCK_RECOVERIES
            .render_value(&mut s, crate::store::poisoned_lock_recoveries());
        catalogue::worker::REAPER_VERDICTS.render_value(&mut s, self.reaper_verdicts_total());
        catalogue::worker::REAPER_DISCHARGED.render_value(&mut s, self.reaper_discharged_total());
        // ── replication (peer-to-peer HA) — own namespace, distinct from the
        // data-path counters above so an HA failure can be localised by layer. ──
        catalogue::worker::REPL_FLUSH_PROPAGATED
            .render_value(&mut s, self.repl_flush_propagated_total());
        catalogue::worker::REPL_TAKEOVER_RESOLVED
            .render_value(&mut s, self.repl_takeover_resolved_total());
        catalogue::worker::REPL_TAKEOVER_HYDRATED
            .render_value(&mut s, self.repl_takeover_hydrated_total());
        catalogue::worker::REPL_REVERSE_FLUSH_REFUSED
            .render_value(&mut s, self.repl_reverse_flush_refused_total());
        catalogue::worker::REPL_TAKEOVER_REFUSED_TERMINATED
            .render_value(&mut s, self.repl_takeover_refused_terminated_total());
        catalogue::worker::REPL_RECLAIMED.render_value(&mut s, self.repl_reclaimed_total());
        catalogue::worker::REPL_SELF_RELEASE.render_value(&mut s, self.repl_self_release_total());
        catalogue::worker::REPL_TERMINAL_LOST.render_value(&mut s, self.repl_terminal_lost_total());
        catalogue::worker::REPL_BOOTSTRAP_SEEDED
            .render_value(&mut s, self.repl_bootstrap_seeded_total());
        catalogue::worker::REPL_BOOTSTRAP_STALLED
            .render_value(&mut s, self.repl_bootstrap_stalled_total());

        // Per-method SIP traffic, the repeats and give-ups, replication per
        // stream, the drains.
        let i = &self.inner;
        let rows = &i.rows;
        rows.requests.render(&mut s);
        rows.responses.render(&mut s);
        rows.requests_out.render(&mut s);
        rows.retransmits.render(&mut s);
        rows.unroutable_dropped.render(&mut s);
        rows.unroutable_refused.render(&mut s);
        rows.unroutable_internal.render(&mut s);
        rows.repeat_give_ups.render(&mut s);
        rows.repl_quiet_turns.render(&mut s);
        rows.repl_applied.render(&mut s);
        rows.repl_noops_sent.render(&mut s);
        rows.repl_forward_flush_refused.render(&mut s);
        catalogue::worker::DRAIN_EXITS.render(&mut s, |series| {
            let exit = DrainExit::ALL[series.index(&catalogue::worker::DRAIN_REASON)];
            i.drain_exits[exit as usize].load(Ordering::Relaxed)
        });
        catalogue::worker::DRAIN_SECONDS.render_histogram(&mut s, |_| HistogramValue {
            buckets: DRAIN_BUCKETS
                .iter()
                .zip(&i.drain_seconds_buckets)
                .map(|(le, n)| (*le, n.load(Ordering::Relaxed)))
                .collect(),
            sum: i.drain_seconds_sum_ms.load(Ordering::Relaxed) as f64 / 1_000.0,
            count: i.drain_seconds_count.load(Ordering::Relaxed),
        });
        self.inner.limiter.render(&mut s);
        catalogue::worker::CALL_REMOVALS_BY_CLASS.render(&mut s, |series| {
            self.removals_of_total(
                RemovalClass::ALL[series.index(&catalogue::worker::REMOVAL_CLASS)],
            )
        });
        catalogue::worker::DISPATCH_PAST_BOUND.render(&mut s, |series| {
            self.past_bound_of_total(PAST_BOUNDS[series.index(&catalogue::worker::PAST_BOUND)])
        });
        catalogue::worker::DISPATCH_OVERFLOW_REFUSED
            .render_value(&mut s, self.overflow_refused_total());
        catalogue::worker::DISPATCH_OVERFLOW_DEPTH.render_value(&mut s, self.overflow_depth());
        catalogue::worker::CALLS_NEAR_LIFETIME_CAP
            .render_value(&mut s, self.calls_near_lifetime_cap());
        catalogue::DISPATCH_INVITE_DISCARD_ANSWERED.render(&mut s, |site| {
            self.invite_discard_answered_of_total(DISCARD_SITES[site.index(&catalogue::SITE)])
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

// ---------------------------------------------------------------------------
// UdpTransportMetrics — the `UdpTransport` facade's Prometheus-visible shape
// ---------------------------------------------------------------------------

/// A live-read source for an endpoint gauge (`queueDepth`, `dropsTailDrop`,
/// `sendWouldBlock`, `kernelRxDropped`). `Arc<dyn Fn>` so the surface is decoupled from the
/// concrete `UdpEndpoint` type (which is held as a `Box<dyn UdpEndpoint>` by the
/// runner and is not `Clone`); the closure captures a clone of the shared
/// counter/queue handle and reads it on demand. `Send + Sync` because the
/// `/metrics` scrape may run on any task.
pub type LiveGauge = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The `UdpTransport` facade's Prometheus-visible shape: a bag of **live
/// getters**. Both the scrape endpoint and test reads want the *instantaneous*
/// value, never a cached snapshot, so every facet reads through on each access:
///
///   - `queue_depth` / `drops_tail_drop` → injected [`LiveGauge`]s backed by the
///     underlying [`UdpEndpoint`](sip_net::UdpEndpoint) (`endpoint.queueDepth()` /
///     `endpoint.counters().tail_dropped`).
///   - `queue_max` → the bind's configured bound (a constant, copied once).
///   - `kernel_rx_dropped` → an injected [`LiveGauge`] over
///     `endpoint.counters().kernel_rx_dropped`: datagrams the kernel dropped
///     before the queue saw them, which no other facet counts.
///   - `ingress_brake_emergency_bypassed` → the shared [`IngressBrakeCounters`] the
///     `preIngress` hook mutates; its refusals are new-call counts
///     ([`crate::new_calls`]).
///
/// Clone-cheap (all fields are `Arc`/`Copy`): the runner keeps one to render
/// `/metrics` and may hand clones to other readers. The runner builds it from
/// the bound endpoint + brake counters and concatenates
/// [`Self::prometheus_text`] into the `/metrics` body.
#[derive(Clone)]
pub struct UdpTransportMetrics {
    queue_depth: LiveGauge,
    queue_max: usize,
    drops_tail_drop: LiveGauge,
    /// Outbound datagrams the socket refused because its send buffer was
    /// full (ADR-0033): a live getter over the bound endpoint.
    send_would_block: LiveGauge,
    /// Datagrams the kernel dropped on the socket before the receive pump read
    /// them (a full `SO_RCVBUF`): a live getter over the bound endpoint.
    kernel_rx_dropped: LiveGauge,
    brake: IngressBrakeCounters,
}

impl std::fmt::Debug for UdpTransportMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Closures aren't Debug — render the live values instead.
        f.debug_struct("UdpTransportMetrics")
            .field("queue_depth", &self.queue_depth())
            .field("queue_max", &self.queue_max)
            .field("drops_tail_drop", &self.drops_tail_drop())
            .field("send_would_block", &self.send_would_block())
            .field("kernel_rx_dropped", &self.kernel_rx_dropped())
            .field("ingress_brake_emergency_bypassed", &self.ingress_brake_emergency_bypassed())
            .finish()
    }
}

impl UdpTransportMetrics {
    /// Build the shape from its live sources — the registry-side of the TS
    /// `UdpTransport.layer` (`const metrics: UdpTransportMetrics = { … }`).
    ///
    ///   - `queue_max`: the bind's configured queue bound (`config.udpQueueMax`).
    ///   - `brake`: the [`IngressBrakeCounters`] the `preIngress` hook holds (so
    ///     `ingress_brake_emergency_bypassed` reads live).
    ///   - `queue_depth` / `drops_tail_drop`: live getters over the bound
    ///     endpoint (typically `move || endpoint.queue_depth()` and
    ///     `move || endpoint.counters().tail_dropped` with a shared handle).
    ///   - `send_would_block` / `kernel_rx_dropped`: live getters over the same
    ///     endpoint's `counters()`.
    pub fn new(
        queue_max: usize,
        brake: IngressBrakeCounters,
        queue_depth: LiveGauge,
        drops_tail_drop: LiveGauge,
        send_would_block: LiveGauge,
        kernel_rx_dropped: LiveGauge,
    ) -> Self {
        Self { queue_depth, queue_max, drops_tail_drop, send_would_block, kernel_rx_dropped, brake }
    }

    /// Live inbound-queue depth (`endpoint.queueDepth()`).
    pub fn queue_depth(&self) -> u64 {
        (self.queue_depth)()
    }
    /// The configured inbound-queue bound (`config.udpQueueMax`).
    pub fn queue_max(&self) -> usize {
        self.queue_max
    }
    /// Datagrams the full inbound queue tail-dropped (`dropsTailDrop`, live ←
    /// `endpoint.counters.tailDropped`).
    pub fn drops_tail_drop(&self) -> u64 {
        (self.drops_tail_drop)()
    }
    /// Outbound datagrams refused by a full send buffer (never a suspended
    /// send — ADR-0033).
    pub fn send_would_block(&self) -> u64 {
        (self.send_would_block)()
    }
    /// Datagrams the kernel dropped before the receive pump read them.
    pub fn kernel_rx_dropped(&self) -> u64 {
        (self.kernel_rx_dropped)()
    }
    /// New emergency INVITEs that bypassed the ingress brake above the threshold.
    pub fn ingress_brake_emergency_bypassed(&self) -> u64 {
        self.brake.emergency_bypassed()
    }
    /// The brake's counters, for the new-call outcome count
    /// ([`crate::new_calls::NewCallCounts::read`]).
    pub fn brake(&self) -> &IngressBrakeCounters {
        &self.brake
    }

    /// Render the shape as Prometheus text exposition for the `/metrics` body —
    /// the registry-visible surface of the TS `registry.udp = metrics`
    /// assignment. All series use the `b2bua_udp_*` namespace.
    ///
    /// Counters (monotonic): `ingress_brake_emergency_bypassed`, `tail_dropped`,
    /// `send_would_block`, `kernel_rx_dropped`. Gauges (instantaneous):
    /// `queue_depth`, `queue_max`. The brake's refusals are new-call counts
    /// (`b2bua_new_calls_total`), rendered with the other admission sources.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(1536);

        // ── Ingress brake: emergency INVITEs let through above the threshold ──
        catalogue::UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED
            .render_value(&mut s, self.ingress_brake_emergency_bypassed());

        // ── Inbound queue state (port of UdpTransportMetrics.queueDepth /
        //    queueMax / dropsTailDrop — live getters over the endpoint). A
        //    tail-dropping queue otherwise shows 100% accepted, hiding a burst
        //    collapse. ──
        catalogue::udp::QUEUE_DEPTH.render_value(&mut s, self.queue_depth());
        catalogue::udp::QUEUE_MAX.render_value(&mut s, self.queue_max() as u64);
        catalogue::udp::TAIL_DROPPED.render_value(&mut s, self.drops_tail_drop());
        catalogue::udp::SEND_WOULD_BLOCK.render_value(&mut s, self.send_would_block());
        catalogue::udp::KERNEL_RX_DROPPED.render_value(&mut s, self.kernel_rx_dropped());
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
    fn unroutable_series_are_published_at_zero_from_startup() {
        let txt = B2buaMetrics::new().prometheus_text();
        assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"ACK\"} 0"));
        assert!(txt.contains("b2bua_unroutable_refused_total{method=\"BYE\",code=\"481\"} 0"));
        assert!(
            txt.contains("b2bua_unroutable_internal_total{event=\"timeout\",method=\"OPTIONS\"} 0")
        );
    }

    #[test]
    fn unroutable_classes_render_apart() {
        let m = B2buaMetrics::new();
        m.record_unroutable_dropped("ACK");
        m.record_unroutable_dropped("2xx");
        m.record_unroutable_refused("BYE", 481);
        m.record_unroutable_internal("timeout", "OPTIONS");
        assert_eq!(m.unroutable_dropped_total(), 2, "only wire messages left unanswered");
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"ACK\"} 1"));
        assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"2xx\"} 1"));
        assert!(txt.contains("b2bua_unroutable_refused_total{method=\"BYE\",code=\"481\"} 1"));
        assert!(
            txt.contains("b2bua_unroutable_internal_total{event=\"timeout\",method=\"OPTIONS\"} 1")
        );
    }

    #[test]
    fn drain_exits_and_duration_render() {
        use crate::drain::{DrainExit, DrainOutcome};
        use crate::limiter::release_queue::ReleaseFlush;
        let m = B2buaMetrics::new();
        let flush =
            ReleaseFlush { queued: 2, given_up: 0, elapsed: std::time::Duration::from_millis(300) };
        m.record_drain_exit(&DrainOutcome {
            exit: DrainExit::CaughtUp,
            residual: 1,
            elapsed: std::time::Duration::from_millis(1_200),
            release_flush: flush,
        });
        m.record_drain_exit(&DrainOutcome {
            exit: DrainExit::GracePeersBehind,
            residual: 1,
            elapsed: std::time::Duration::from_secs(5),
            release_flush: ReleaseFlush::default(),
        });
        assert_eq!(
            m.limiter()
                .release_flushes_total(crate::limiter::release_queue::ReleaseFlushOutcome::Sent),
            1
        );
        assert_eq!(
            m.limiter()
                .release_flushes_total(crate::limiter::release_queue::ReleaseFlushOutcome::Empty),
            1
        );
        assert_eq!(m.drain_exits("caught_up"), 1);
        assert_eq!(m.drain_exits("grace_peers_behind"), 1);
        assert_eq!(m.drain_exits("quiescent"), 0);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_drain_exits_total{reason=\"caught_up\"} 1"));
        assert!(txt.contains("b2bua_drain_exits_total{reason=\"grace_peers_behind\"} 1"));
        // 1.2 s falls in every bucket from 2 up; 5 s in the 5 and 10 buckets.
        assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"1\"} 0"));
        assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"2\"} 1"));
        assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"5\"} 2"));
        assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"+Inf\"} 2"));
        assert!(txt.contains("b2bua_drain_seconds_sum 6.2"));
        assert!(txt.contains("b2bua_drain_seconds_count 2"));
        assert!(txt.contains("b2bua_limiter_release_flushes_total{outcome=\"sent\"} 1"));
        assert!(txt.contains("b2bua_limiter_release_flush_seconds_total 0.3"));
    }

    #[test]
    fn retransmit_and_give_up_families_render() {
        let m = B2buaMetrics::new();
        m.record_retransmit("final-2xx", "invite", Some(200));
        m.record_retransmit("final-2xx", "INVITE", Some(200));
        m.record_retransmit("reliable-provisional", "INVITE", Some(183));
        m.record_retransmit("trigger", "ACK", None);
        m.record_repeat_give_up("ack-of-2xx");
        m.record_quiet_turn("own-rung");
        m.record_quiet_turn("own-rung");
        assert_eq!(m.repl_quiet_turns_total("own-rung"), 2);
        assert_eq!(m.repl_quiet_turns_total("re-ack"), 0);
        assert_eq!(m.retransmits_total("final-2xx", "INVITE", Some(200)), 2);
        assert_eq!(m.retransmits_total("trigger", "ACK", None), 1);
        assert_eq!(
            m.retransmits_total("trigger", "ACK", Some(200)),
            0,
            "a request row carries no code"
        );
        assert_eq!(m.repeat_give_ups_total("ack-of-2xx"), 1);
        assert_eq!(m.repeat_give_ups_total("prack-of"), 0);
        let txt = m.prometheus_text();
        assert!(
            txt.contains(
                "b2bua_retransmits_total{ladder=\"final-2xx\",method=\"INVITE\",code=\"200\"} 2"
            ),
            "{txt}"
        );
        assert!(txt.contains("b2bua_retransmits_total{ladder=\"reliable-provisional\",method=\"INVITE\",code=\"183\"} 1"));
        assert!(
            txt.contains("b2bua_retransmits_total{ladder=\"trigger\",method=\"ACK\"} 1"),
            "no code label on a request: {txt}"
        );
        assert!(txt.contains("b2bua_repeat_give_ups_total{obligation=\"ack-of-2xx\"} 1"));
        assert!(txt.contains("b2bua_repl_quiet_turns_total{kind=\"own-rung\"} 2"), "{txt}");
    }

    #[test]
    fn clock_wall_divergence_gauge_round_trips_signed_and_renders() {
        let m = B2buaMetrics::new();
        // Unset → 0 (flat gauge).
        assert_eq!(m.clock_wall_divergence_ms(), 0);
        assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms 0"));
        // A forward host step (positive) round-trips.
        m.set_clock_wall_divergence_ms(300_000);
        assert_eq!(m.clock_wall_divergence_ms(), 300_000);
        assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms 300000"));
        // A backward step (negative) survives the u64-bit-pattern storage.
        m.set_clock_wall_divergence_ms(-45_000);
        assert_eq!(m.clock_wall_divergence_ms(), -45_000);
        assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms -45000"));
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
        assert!(txt.contains("b2bua_withdrawn_running 0"));
        m.set_withdrawn_running(true);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_withdrawn_running 1"));
        assert!(txt.contains("# TYPE b2bua_withdrawn_running gauge"));
        m.set_repl_peers_pulled_not_ready(1);
        let txt = m.prometheus_text();
        assert!(txt.contains("b2bua_repl_peers_pulled_not_ready 1"));
        assert!(txt.contains("# TYPE b2bua_repl_peers_pulled_not_ready gauge"));
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
    /// brake counters. Mirrors the TS shape's
    /// live getters without standing up a fabric — the fabric-proxy facet is
    /// covered end-to-end in `tests/it/udp_transport_metrics.rs`.
    fn shape() -> (UdpTransportMetrics, IngressBrakeCounters, Arc<AtomicU64>, Arc<AtomicU64>) {
        let brake = IngressBrakeCounters::new();
        let depth = Arc::new(AtomicU64::new(0));
        let tail = Arc::new(AtomicU64::new(0));
        let d = depth.clone();
        let t = tail.clone();
        let m = UdpTransportMetrics::new(
            5, // queue_max — the brake test's QUEUE_MAX
            brake.clone(),
            Arc::new(move || d.load(Ordering::Relaxed)),
            Arc::new(move || t.load(Ordering::Relaxed)),
            Arc::new(|| 0),
            Arc::new(|| 7),
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
        assert_eq!(m.ingress_brake_emergency_bypassed(), 0);
        assert_eq!(m.drops_tail_drop(), 0);

        // Mutate the sources — the shape reflects them live.
        depth.store(2, Ordering::Relaxed);
        tail.store(7, Ordering::Relaxed);
        brake.record_emergency_bypass();
        brake.record_emergency_bypass();
        brake.record_emergency_bypass();
        assert_eq!(m.queue_depth(), 2);
        assert_eq!(m.drops_tail_drop(), 7);
        assert_eq!(m.ingress_brake_emergency_bypassed(), 3);
    }

    /// The render carries every field of the shape, with the right Prometheus
    /// TYPE per field (counters for the monotonic sheds/tail-drops/buffered,
    /// gauges for the instantaneous depth/max/peers).
    #[test]
    fn udp_transport_metrics_render() {
        let (m, _brake, depth, tail) = shape();
        depth.store(3, Ordering::Relaxed);
        tail.store(11, Ordering::Relaxed);
        let txt = m.prometheus_text();

        // Emergency-bypass visibility series (renders even at 0).
        assert!(txt.contains("b2bua_udp_ingress_brake_emergency_bypassed_total 0"));
        assert!(txt.contains("# TYPE b2bua_udp_ingress_brake_emergency_bypassed_total counter"));
        // Live queue facets.
        assert!(txt.contains("b2bua_udp_queue_depth 3"));
        assert!(txt.contains("b2bua_udp_queue_max 5"));
        assert!(txt.contains("b2bua_udp_tail_dropped_total 11"));
        assert!(txt.contains("b2bua_udp_kernel_rx_dropped_total 7"));
        assert!(txt.contains("# TYPE b2bua_udp_kernel_rx_dropped_total counter"));
        // Prometheus TYPE lines: gauges vs counters.
        assert!(txt.contains("# TYPE b2bua_udp_queue_depth gauge"));
        assert!(txt.contains("# TYPE b2bua_udp_queue_max gauge"));
        assert!(txt.contains("# TYPE b2bua_udp_tail_dropped_total counter"));
    }
}
