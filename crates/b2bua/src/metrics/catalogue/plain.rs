//! The worker's plain counters: no labels, bumped by one, read whole. Each is
//! declared once, by a row naming its family (metric name and help), its
//! storage and its two accessors; the row gives the family, the storage, the
//! accessors, the render line, and the family's place at the head of
//! [`WORKER`](super::WORKER), in row order. Adding one is a row here and
//! `METRIC_CONTRACT_BLESS=1` on metric-contract's `catalogue.names`.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_catalogue::{Family, Labels};

use crate::metrics::B2buaMetrics;

macro_rules! plain_counters {
    ($( $family:ident = ($name:literal, $help:literal $(,)?) => $field:ident, $bump:ident, $get:ident; )+) => {
        $( pub const $family: Family = Family::counter($name, Labels::None, $help); )+

        /// Every plain counter's family, in row order.
        pub const FAMILIES: &[Family] = &[$($family),+];

        /// The plain counters' storage.
        #[derive(Debug, Default)]
        pub(in crate::metrics) struct PlainCounters {
            $( $field: AtomicU64, )+
        }

        impl B2buaMetrics {
            $(
                pub fn $bump(&self) {
                    self.inner.plain.$field.fetch_add(1, Ordering::Relaxed);
                }

                pub fn $get(&self) -> u64 {
                    self.inner.plain.$field.load(Ordering::Relaxed)
                }
            )+
        }

        /// Writes every plain counter, in row order.
        pub(in crate::metrics) fn render(metrics: &B2buaMetrics, s: &mut String) {
            $( $family.render_value(s, metrics.$get()); )+
        }
    };
}

plain_counters! {
    MESSAGE_CAP_LIFETIME_CROSSED = (
        "b2bua_message_cap_lifetime_crossed_total",
        "calls that crossed max_messages_per_call_lifetime: more events offered over the call's life than any healthy call sees, counted at dispatch until the call's release is queued; each is sent one verdict that ends it",
    ) => message_cap_lifetime_crossed, bump_message_cap_lifetime_crossed, message_cap_lifetime_crossed_total;
    DISPATCH_CAPPED_REFUSALS = (
        "b2bua_dispatch_capped_refusals_total",
        "events refused on a call past its lifetime cap: a BYE is answered 200 where refused, any other request but ACK 481 (a CANCEL matching no transaction statelessly); responses and Cancelled notices keep their room and are never refused; a new INVITE is counted on b2bua_new_calls_total instead",
    ) => capped_refusals, bump_capped_refusal, capped_refusals_total;
    DISPATCH_CAPPED_REQUEST_ANSWERED = (
        "b2bua_dispatch_capped_request_answered_total",
        "non-INVITE requests refused on a call past its lifetime cap and answered where refused: BYE 200, any other 481",
    ) => capped_request_answered, bump_capped_request_answered, capped_request_answered_total;
    // MAX_MESSAGES_PER_CALL cap-defense: calls torn down for crossing the
    // per-call message cap (a runaway re-INVITE/OPTIONS storm or glare loop),
    // so no call processes unbounded in-dialog events.
    MESSAGE_CAP_TERMINATED = (
        "b2bua_message_cap_terminated_total",
        "calls terminated for exceeding max_messages_per_call (cap-defense; a climbing rate names a runaway-traffic call class)",
    ) => message_cap_terminated, bump_message_cap_terminated, message_cap_terminated_total;
    DISPATCH_QUEUE_DROPS = (
        "b2bua_dispatch_queue_drops_total",
        "events dropped: per-call queue full (a non-INVITE request's transaction is forgotten so its retransmission is admitted again; an in-dialog INVITE is answered with a Retry-After); a new INVITE is counted on b2bua_new_calls_total instead",
    ) => queue_drops, bump_queue_drop, queue_drops_total;
    DISPATCH_CAP_DROPS = (
        "b2bua_dispatch_cap_drops_total",
        "events hitting the global call cap with no queue for their call: a non-INVITE request's transaction is forgotten so its retransmission is admitted again, an in-dialog INVITE is answered 500 with a Retry-After; a new INVITE is counted on b2bua_new_calls_total instead",
    ) => cap_drops, bump_cap_drop, cap_drops_total;
    DISPATCH_RELEASE_DISCARDS = (
        "b2bua_dispatch_release_discards_total",
        "events offered behind a call's release with no new call waiting there, discarded unrun; a new INVITE, and every event after it, waits for the call's next queue instead",
    ) => release_discards, bump_release_discard, release_discards_total;
    DISPATCH_SATURATION = (
        "b2bua_dispatch_saturation_total",
        "handler bodies that waited for a permit of the shared pool (global handler concurrency)",
    ) => saturation, bump_saturation, saturation_total;
    DISPATCH_NEW_CALL_SHARE_WAITS = (
        "b2bua_dispatch_new_call_share_waits_total",
        "new-call handler bodies that waited for a permit of the new-call share",
    ) => new_call_share_waits, bump_new_call_share_wait, new_call_share_waits_total;
    HANDLER_TIMEOUTS = (
        "b2bua_handler_timeouts_total",
        "handler executions that timed out",
    ) => handler_timeouts, bump_handler_timeout, handler_timeouts_total;
    FORCE_PURGE = (
        "b2bua_force_purge_total",
        "calls force-purged (loop guard)",
    ) => force_purge, bump_force_purge, force_purge_total;
    FAST_REJECT_TERMINATING = (
        "b2bua_fast_reject_terminating_total",
        "requests fast-rejected on a terminating call",
    ) => fast_reject_terminating, bump_fast_reject_terminating, fast_reject_terminating_total;
    CDR_WRITTEN = (
        "b2bua_cdr_written_total",
        "CDRs the sink delivered (for a broker sink: acked by the broker)",
    ) => cdr_written, bump_cdr_written, cdr_written_total;
    CDR_DROPPED = (
        "b2bua_cdr_dropped_total",
        "CDRs dropped (submit-queue overflow, or a record the sink did not deliver)",
    ) => cdr_dropped, bump_cdr_dropped, cdr_dropped_total;
    // A `/call/new` decision result (route or reject) that landed on a call
    // whose initial INVITE the caller already CANCELed — recorded on the call,
    // never applied: no b-leg launch, no second final on the a-leg's completed
    // transaction. A non-zero rate measures the caller-gives-up-
    // during-routing race, not a fault.
    DECISION_DROPPED_CANCELLED = (
        "b2bua_decision_dropped_cancelled_total",
        "decision results (route/reject) recorded and not applied because the caller CANCELed the initial INVITE while the decision was in flight (the 487 is the transaction's one final; no b-leg is launched)",
    ) => decision_dropped_cancelled, bump_decision_dropped_cancelled, decision_dropped_cancelled_total;
    TERMINATION_UNRECORDED = (
        "b2bua_termination_unrecorded_total",
        "calls that reached Terminated with no termination record (a path to terminal states no cause) — expected 0",
    ) => termination_unrecorded, bump_termination_unrecorded, termination_unrecorded_total;
    // Second-final refusal (RFC 3261 §17.2.1): a final the call layer authored
    // toward the a-leg's initial INVITE while that transaction already carried
    // one — refused at the a-leg response seam, never built. A rule made
    // progress on a call already answered or going away; expected 0 in any
    // healthy run.
    SECOND_FINAL_REFUSED = (
        "b2bua_second_final_refused_total",
        "finals toward the a-leg's initial INVITE refused because that transaction already carries one (RFC 3261 §17.2.1); a rule made progress on an answered or going-away call — expected 0",
    ) => second_final_refused, bump_second_final_refused, second_final_refused_total;
    // Late-provisional refusal (RFC 3261 §13.3.1.1 / §17.2.1): a provisional
    // the call layer authored toward the a-leg's initial INVITE while that
    // transaction already sent its final — refused at the a-leg response
    // seam, never built. A rule showed a ringing leg to an answered caller;
    // expected 0 in any healthy run.
    PROVISIONAL_AFTER_FINAL_REFUSED = (
        "b2bua_provisional_after_final_refused_total",
        "provisionals toward the a-leg's initial INVITE refused because that transaction already sent its final (RFC 3261 §13.3.1.1 / §17.2.1); a rule showed a ringing leg to an answered caller — expected 0",
    ) => provisional_after_final_refused, bump_provisional_after_final_refused, provisional_after_final_refused_total;
    // Going-away gate: an asynchronous trigger (timer fire, transaction
    // timeout, internal-event fold) that landed on a Terminating/Terminated
    // call and matched a rule that is not a teardown rule — absorbed before
    // it ran. Measures the race between a call's own clocks and its teardown,
    // not a fault; a rule named here on a live call would have made progress.
    GOING_AWAY_ABSORBED = (
        "b2bua_going_away_absorbed_total",
        "asynchronous triggers (timer fire / transaction timeout / internal-event fold) absorbed on a Terminating or Terminated call because the rule they matched is not a teardown rule; the race between a call's own clocks and its teardown, not a fault",
    ) => going_away_absorbed, bump_going_away_absorbed, going_away_absorbed_total;
    LATE_PRACK_ANSWERED = (
        "b2bua_late_prack_answered_total",
        "PRACKs that reached this node after their call's release and named a reliable provisional it showed, answered 200 (RFC 3262 §3); the caller's PRACK crossing the final",
    ) => late_prack_answered, bump_late_prack_answered, late_prack_answered_total;
    // An initial INVITE the front proxy placed here as another primary's
    // backup (its cookie names that primary) that is no copy of a replica
    // this node holds: the primary owns its transaction, so it is dropped
    // with no final rather than placed as a second call.
    FOREIGN_INVITE_COPY_DROPPED = (
        "b2bua_foreign_invite_copies_dropped_total",
        "initial INVITEs whose stickiness cookie names another worker as primary and that copy no replica this node holds, dropped with no final (the transaction layer's 100 Trying went out on receipt; the transaction is then relinquished): the primary owns the transaction, so placing them would dial a second call",
    ) => foreign_invite_copy_dropped, bump_foreign_invite_copy_dropped, foreign_invite_copies_dropped_total;
    // Another incarnation's event: a timer fire, transaction timeout,
    // callout result or late message of an earlier call on the same
    // callRef, dropped before the live call's rules read it. Measures the
    // race between a call's end and a retry born on its callRef, not a fault.
    OTHER_INCARNATION_DROPPED = (
        "b2bua_other_incarnation_dropped_total",
        "events of an earlier call on the same callRef (timer fire / transaction timeout / callout result / late message) dropped before the live call's rules read them; the race between a call's end and a retry born on its callRef, not a fault",
    ) => other_incarnation_dropped, bump_other_incarnation_dropped, other_incarnation_dropped_total;
    // Injectable store-fault seam (ADR-0023): `store_fault_rejected` = live
    // lookups that failed CLOSED with a 500 final (initial-INVITE dialog-
    // existence check or in-dialog request fetch); `store_fault_audit_skipped`
    // = keepalive/audit cycles skipped FAIL-OPEN (call kept up, timer re-armed
    // so liveness detection resumes next interval — a store fault alone never
    // tears down an established call). Both stay 0 unless a `StoreFaults`
    // handle is armed.
    STORE_FAULT_REJECTED = (
        "b2bua_store_fault_rejected_total",
        "live store lookups that failed CLOSED (a 500 final to the initial INVITE or in-dialog request; a faulted ACK is dropped un-answered; 0 unless a fault is armed)",
    ) => store_fault_rejected, bump_store_fault_rejected, store_fault_rejected_total;
    STORE_FAULT_AUDIT_SKIPPED = (
        "b2bua_store_fault_audit_skipped_total",
        "keepalive/audit cycles skipped FAIL-OPEN on a store fault (call kept up, timer re-armed; 0 unless a fault is armed)",
    ) => store_fault_audit_skipped, bump_store_fault_audit_skipped, store_fault_audit_skipped_total;
    // Call reaper (ADR-0020): `handler_panics` counts dispatcher-observed body
    // panics; `reaper_verdicts` counts injected synthetic events (stale + fatal +
    // discharge); `reaper_discharged` is the ALARM — the rules path itself
    // failed twice for a call; expected ~0 in any healthy run.
    HANDLER_PANICS = (
        "b2bua_handler_panics_total",
        "handler bodies that panicked (dispatcher-observed; each becomes a reaper strike instead of a silent call leak)",
    ) => handler_panics, bump_handler_panic, handler_panics_total;
    REAPER_SWEEPS = (
        "b2bua_reaper_sweeps_total",
        "reaper sweep ticks executed",
    ) => reaper_sweeps, bump_reaper_sweep, reaper_sweeps_total;
    REAPER_SWEEP_PANICS = (
        "b2bua_reaper_sweep_panics_total",
        "reaper sweep steps that panicked; the next pass runs one interval later — expected 0",
    ) => reaper_sweep_panics, bump_reaper_sweep_panic, reaper_sweep_panics_total;
    REPL_FRAMES_REFUSED = (
        "b2bua_repl_frames_refused_total",
        "replication frames the transport refused unsent (past the frame cap); each is skipped and the stream goes on — expected 0",
    ) => repl_frames_refused, bump_repl_frame_refused, repl_frames_refused_total;
    REPL_STREAMS_REFUSED = (
        "b2bua_repl_streams_refused_total",
        "replication streams a peer refused at connect for another protocol version; the puller pulls again after its backoff — expected 0",
    ) => repl_streams_refused, bump_repl_stream_refused, repl_streams_refused_total;
    REPL_NEW_CALLS_UNBACKED = (
        "b2bua_repl_new_calls_unbacked_total",
        "new calls admitted with no backup, for life, while the backlog toward their backup was over its high-water mark (ADR-0055 §3)",
    ) => repl_new_calls_unbacked, bump_repl_new_call_unbacked, repl_new_calls_unbacked_total;
    REPL_SYNC_CAPPED = (
        "b2bua_repl_sync_capped_total",
        "outbox entries released at the sync cap, before their backup applied the write that records them (ADR-0055 §3)",
    ) => repl_sync_capped, bump_repl_sync_capped, repl_sync_capped_total;
    DEPARTURE_TAKEOVER_PANICS = (
        "b2bua_departure_takeover_panics_total",
        "departed-peer calls whose takeover panicked; the sweep goes on with the next call — expected 0",
    ) => departure_takeover_panics, bump_departure_takeover_panic, departure_takeover_panics_total;
    DEAD_NOTICES_REFUSED = (
        "b2bua_dead_notices_refused_total",
        "a front proxy's dead notices (X-Worker-Dead) refused unsigned, badly signed, stale, or with no cluster key to check them (ADR-0050 §3) — expected 0",
    ) => dead_notices_refused, bump_dead_notice_refused, dead_notices_refused_total;
    DEAD_VERDICTS_TAKEN = (
        "b2bua_dead_verdicts_taken_total",
        "a front proxy's dead verdicts on another worker taken: each runs the takeover of the calls this node backs for it with work in flight (ADR-0047 D1)",
    ) => dead_verdicts_taken, bump_dead_verdict_taken, dead_verdicts_taken_total;
    STORE_RECONCILED_ORPHAN_STAMPS = (
        "b2bua_store_reconciled_orphan_stamps_total",
        "last-touched stamps of no resident call the reaper sweep dropped; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_orphan_stamps, bump_store_reconciled_orphan_stamp, store_reconciled_orphan_stamps_total;
    STORE_RECONCILED_MISSING_STAMPS = (
        "b2bua_store_reconciled_missing_stamps_total",
        "resident calls with no last-touched stamp the reaper sweep stamped; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_missing_stamps, bump_store_reconciled_missing_stamp, store_reconciled_missing_stamps_total;
    STORE_RECONCILED_ORPHAN_INDEX = (
        "b2bua_store_reconciled_orphan_index_total",
        "routing-index entries of no resident call the reaper sweep dropped; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_orphan_index, bump_store_reconciled_orphan_index, store_reconciled_orphan_index_total;
    STORE_RECONCILED_MISSING_INDEX = (
        "b2bua_store_reconciled_missing_index_total",
        "resident calls with no routing-index key list the reaper sweep indexed again, never taking a key another call holds; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_missing_index, bump_store_reconciled_missing_index, store_reconciled_missing_index_total;
    STORE_RECONCILED_ORPHAN_TAKEOVER = (
        "b2bua_store_reconciled_orphan_takeover_total",
        "takeover marks of no resident call the reaper sweep dropped; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_orphan_takeover, bump_store_reconciled_orphan_takeover, store_reconciled_orphan_takeover_total;
    STORE_RECONCILED_ORPHAN_ENVELOPE = (
        "b2bua_store_reconciled_orphan_envelope_total",
        "store envelopes of no resident call the reaper sweep dropped; each is a store write a panic cut short — expected 0",
    ) => store_reconciled_orphan_envelope, bump_store_reconciled_orphan_envelope, store_reconciled_orphan_envelope_total;
    REPLICA_REAP_PANICS = (
        "b2bua_replica_reap_panics_total",
        "replica reap steps that panicked; the next pass runs one interval later — expected 0",
    ) => replica_reap_panics, bump_replica_reap_panic, replica_reap_panics_total;
    REAPER_VERDICTS = (
        "b2bua_reaper_verdicts_total",
        "reaper verdicts injected (stale + fatal-error + discharge synthetic events)",
    ) => reaper_verdicts, bump_reaper_verdict, reaper_verdicts_total;
    REAPER_DISCHARGED = (
        "b2bua_reaper_discharged_total",
        "strike-2 discharges: the rules path itself failed for a call and the snapshot was forced terminal directly (ALARM: expected ~0)",
    ) => reaper_discharged, bump_reaper_discharged, reaper_discharged_total;
    UNRECOVERABLE_CALLS = (
        "b2bua_unrecoverable_calls_total",
        "calls whose record does not rebuild their caller's INVITE, discharged with nothing on the wire (ADR-0052 §11; ALARM: expected 0)",
    ) => unrecoverable_calls, bump_unrecoverable_call, unrecoverable_calls_total;
    UNRECOVERABLE_MEETINGS = (
        "b2bua_unrecoverable_meetings_total",
        "copies of the other owner not taken at a meeting because they do not rebuild the caller's INVITE; the live copy keeps serving (ADR-0052 §11; ALARM: expected 0)",
    ) => unrecoverable_meetings, bump_unrecoverable_meeting, unrecoverable_meetings_total;
    // replication (peer-to-peer HA; separate namespace `b2bua_repl_*`). These
    // localise an HA failure to a layer: `flush_propagated` rising on the PRIMARY
    // proves it is attempting to replicate (the proxy cookie stamped
    // `topology.bak`); the per-stream `applied` breakdown (below) proves the
    // replica actually arrived; `takeover_resolved`/`hydrated` prove a failed-over
    // in-dialog request found + loaded the replica on the backup. The TRUE resident
    // backup count is the sampled `repl_meta_backup` gauge (not a counter-derived
    // estimate).
    REPL_FLUSH_PROPAGATED = (
        "b2bua_repl_flush_propagated_total",
        "primary flushes that propagated to a backup peer (topology.bak set)",
    ) => repl_flush_propagated, bump_repl_flush_propagated, repl_flush_propagated_total;
    // A body is refused on either side of the codec's nesting bound
    // (`call::codec::MAX_DEPTH`): the call it carries has no replica.
    REPL_FLUSH_UNENCODABLE = (
        "b2bua_repl_flush_unencodable_total",
        "flushes skipped because the call body did not encode or nests past the codec's bound: the call has no replica (ALARM: expected 0)",
    ) => repl_flush_unencodable, bump_repl_flush_unencodable, repl_flush_unencodable_total;
    REPL_BODY_UNDECODABLE = (
        "b2bua_repl_body_undecodable_total",
        "replicated call bodies refused at decode: malformed or nested past the codec's bound (ALARM: expected 0)",
    ) => repl_body_undecodable, bump_repl_body_undecodable, repl_body_undecodable_total;
    REPL_TAKEOVER_RESOLVED = (
        "b2bua_repl_takeover_resolved_total",
        "in-dialog requests whose callRef was recovered from the replica index (acting-backup)",
    ) => repl_takeover_resolved, bump_repl_takeover_resolved, repl_takeover_resolved_total;
    REPL_TAKEOVER_HYDRATED = (
        "b2bua_repl_takeover_hydrated_total",
        "calls hydrated from a backup replica to serve a failed-over request",
    ) => repl_takeover_hydrated, bump_repl_takeover_hydrated, repl_takeover_hydrated_total;
    // Reverse copies a live primary's copy contains; the meeting is `router::reclaim`.
    REPL_REVERSE_FLUSH_REFUSED = (
        "b2bua_repl_reverse_flush_refused_total",
        "a backup's reverse flush for a call this primary serves live that the live copy contains (ADR-0055 §7): the primary kept its own copy",
    ) => repl_reverse_flush_refused, bump_repl_reverse_flush_refused, repl_reverse_flush_refused_total;
    // Copies of one call that met diverged; the meeting is `router::reclaim`.
    REPL_COPIES_DIVERGED = (
        "b2bua_repl_copies_diverged_total",
        "a copy of a call this node serves live met the other owner's copy and neither contained the other (ADR-0055 §7): the call was ended cleanly",
    ) => repl_copies_diverged, bump_repl_copies_diverged, repl_copies_diverged_total;
    // Refusals of a `Terminated` replica; the rule is `router::materialise`.
    REPL_TAKEOVER_REFUSED_TERMINATED = (
        "b2bua_repl_takeover_refused_terminated_total",
        "backup-replica lookups refused because the body is Terminated (a released takeover copy): the message falls to the orphan 481/drop instead of re-serving a call that already ended",
    ) => repl_takeover_refused_terminated, bump_repl_takeover_refused_terminated, repl_takeover_refused_terminated_total;
    // Fail-back (ADR-0011 X11 / ADR-0014): `reclaimed` = calls a rebooted primary
    // re-materialised into its live map (active reclaim); `self_release` = acting-
    // backup takeover copies the backup *self-released* once the transaction(s) it
    // served reached a terminal state (ADR-0014). After a kill_worker+reclaim,
    // `self_release` ≈ takeover copies shed and the active/sipp gap reaps to ~0.
    REPL_RECLAIMED = (
        "b2bua_repl_reclaimed_total",
        "calls a rebooted primary re-materialised into its live map + re-armed (active reclaim, ADR-0011 X11)",
    ) => repl_reclaimed, bump_repl_reclaimed, repl_reclaimed_total;
    REPL_SELF_RELEASE = (
        "b2bua_repl_self_release_total",
        "acting-backup takeover copies self-released once their served transaction(s) reached a terminal state (ADR-0014, replaces the Deactivate handback)",
    ) => repl_self_release, bump_repl_self_release, repl_self_release_total;
    REPL_STAND_DOWN = (
        "b2bua_repl_stand_down_total",
        "acting-backup takeover copies stood down on a write of their primary that covers every write the copy made, or on its delete (ADR-0047 D7)",
    ) => repl_stand_down, bump_repl_stand_down, repl_stand_down_total;
    // A backup-held deferred terminal whose primary, still a member, never came
    // back to reclaim it (past the replica TTL). A copy whose primary is a member
    // defers the CDR to it (ADR-0055 §5, ADR-0020 X3), so its periodic reap
    // releases the call's limiter hold(s) + frees the replica memory but writes
    // **no CDR**.
    // This counter is that lost-CDR count: it should stay ~0 in a healthy cluster
    // and only climbs when a primary is permanently lost mid-call.
    REPL_TERMINAL_LOST = (
        "b2bua_repl_terminal_lost_total",
        "backup-held deferred terminals whose primary never reclaimed them (dead past the replica TTL): limiter released + memory freed by the periodic reap, but NO CDR — the accepted lost-CDR double-failure (ADR-0020 X3)",
    ) => repl_terminal_lost, bump_repl_terminal_lost, repl_terminal_lost_total;
    REPL_BOOTSTRAP_SEEDED = (
        "b2bua_repl_bootstrap_seeded_total",
        "rebooted-primary bootstrap passes that reached the first catch-up Noop (peer streamed the full bak:{me} keyset)",
    ) => repl_bootstrap_seeded, bump_repl_bootstrap_seeded, repl_bootstrap_seeded_total;
    REPL_BOOTSTRAP_STALLED = (
        "b2bua_repl_bootstrap_stalled_total",
        "rebooted-primary bootstrap passes that hit the bootstrap hard deadline before the first Noop (best-effort completion; keeps streaming on the same socket)",
    ) => repl_bootstrap_stalled, bump_repl_bootstrap_stalled, repl_bootstrap_stalled_total;
}
