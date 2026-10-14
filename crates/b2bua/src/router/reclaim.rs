//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): the reactive reverse-flush reconcile, on-reboot
//! discharge of deferred terminals; the bulk reboot reclaim and the straggler
//! reclaim live in `bulk`, how a fold lands in `fold`, the periodic
//! replica-store reap in `reap`.

use std::sync::Arc;

use call::helpers::{lifecycle_behind, lifecycle_progress, raise_sequences};
use call::{Call, CallModelState};

use super::interpret::process_result;
use super::RouterCtx;
use crate::store::{role_of, MaterialiseOrigin, PartitionRole};

mod bulk;
mod ending;
mod fold;
mod reap;
pub(super) use bulk::reclaim_all;
use bulk::reclaim_into_live;
use ending::discharge_folded_terminal;
use fold::{fold_quietly, Fold};
pub(crate) use reap::reap_expired_replicas;

/// Apply a backup's reverse-flushed mutation that the puller just landed in our
/// `pri:{self}` partition into our **live** map (ADR-0014 Reclaim-tail
/// reconcile, extended to the live copy). The acting-backup served an in-dialog
/// request for a call **we still own live**; fold its dominating `(p,b)` view in
/// so our copy converges — Model Y: the live primary is the *sole* discharge
/// authority, the backup only defers:
/// - **Terminated** → discharge as our own through the reaper funnel — one CDR,
///   limiter released, the propagated delete evicts the backup's deferred copy.
/// - **Active** (a re-INVITE/UPDATE the backup re-originated) → fold the new state
///   in, notably the bumped b-leg `our_cseq`, so OUR next request to the peer
///   continues the dialog monotonically (C11: no CSeq split).
/// - **Terminating** → transient teardown-in-progress; wait for the Terminated
///   flush rather than fold a half-state.
///
/// Not live: a **Terminated** body is a reboot-reclaimed deferral — materialise +
/// discharge (no keepalive arming); anything else is the existing reactive
/// straggler, materialised + re-armed by [`reclaim_into_live`].
///
/// The reverse gate ([`reverse_fold`]) is re-checked under the
/// per-call lock; a primary that mutated the call since the backup branched
/// keeps its own copy unless the flush carries lifecycle progress it could not
/// have made itself. A fold is not this node's write ([`fold_quietly`]): the
/// live copy then holds the backup's version, so a re-delivered flush no
/// longer dominates and the backup's next write is the next version.
pub(super) async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
    // A read that sees an expired body too: an expired reverse-flushed terminal
    // is still folded and discharged here.
    let Some((first, _)) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        return;
    };
    // Not-live + non-terminal is the reactive-straggler path; it manages
    // its own per-call lock, so route it BEFORE taking the lock here (the guard is
    // not reentrant).
    if ctx.state.peek(call_ref).is_none() && first.state != CallModelState::Terminated {
        reclaim_into_live(ctx, call_ref, None).await;
        return;
    }
    let guard = ctx.state.lock(call_ref).await;
    // Read again under the lock: a turn that held it may have ended the call
    // and deleted the body, its CDR written; the body read before is stale.
    let Some((replica, skew_offset_ms)) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        if ctx.state.peek(call_ref).is_none() {
            drop(guard);
            ctx.state.discard_orphan(call_ref);
        }
        return;
    };
    let now_ms = ctx.clock.now_ms();
    match ctx.state.peek(call_ref) {
        // The stored body is another call than the live one (the release of
        // the stored call is still queued): it is not this call's progress.
        Some(live) if replica.incarnation() != live.incarnation() => {}
        Some(live) => {
            let Some(fold) = reverse_fold(&replica, &live) else {
                ctx.metrics.bump_repl_reverse_flush_refused();
                adopt_seen_counter(ctx, FlushDirection::Reverse, &live, &replica);
                return;
            };
            // Rare by construction (a backup served an in-dialog request for a
            // call we still own), so it gets its own line with the identity an
            // operator correlates by: the call and both `(p,b)` views.
            tracing::info!(
                node = observe::node(),
                call_ref,
                call_id = %live.a_leg.call_id,
                state = ?replica.state,
                replica_pb = %format_pb(&replica),
                live_pb = %format_pb(&live),
                "reverse-flush reconcile"
            );
            match replica.state {
                // The backup deferred a terminal it served; discharge OUR live copy
                // through the funnel with the live (non-terminal) copy as `before`
                // so the `→ Terminated` edge fires the ObligationSet + RemoveCall.
                CallModelState::Terminated => {
                    discharge_folded_terminal(ctx, call_ref, &live, replica, now_ms).await;
                }
                // The backup served the call on: an answer, or a re-INVITE/UPDATE
                // it re-originated (the bumped b-leg `our_cseq`, C11). Fold its
                // state in and let the timer service follow the folded ledger. No
                // discharge — the call continues.
                CallModelState::Active => {
                    fold_quietly(
                        ctx,
                        FlushDirection::Reverse,
                        call_ref,
                        &live,
                        replica,
                        fold,
                        skew_offset_ms,
                        now_ms,
                    )
                    .await;
                }
                // Transient teardown-in-progress; wait for the Terminated flush.
                CallModelState::Terminating => {}
            }
        }
        // A reverse-flushed deferral for a call we no longer hold live (the
        // reactive straggler equivalent of the reboot-reclaim terminal): discharge.
        None => {
            if replica.state == CallModelState::Terminated {
                discharge_materialized_terminal(ctx, call_ref, replica, now_ms, false).await;
            }
        }
    }
}

/// Materialise a not-live deferred-terminal body and discharge it through the ONE
/// funnel with a synthetic non-terminal `before` so `enforce`'s `became_terminated`
/// edge fires (the body is already terminal, so the edge must be synthesised). The
/// single materialise + synth-`before` + enforce + `process_result` site, shared by
/// every "discharge a body we don't hold live" caller:
///   - the reverse-flush reconcile (a backup's deferral for a call we released) and
///     the reboot bulk/on-demand reclaim of a `Terminated` body → `force_terminal =
///     false`: the body already carries its real BYE CDR, so discharge it as-is.
///   - the backup-durable fallback (`reap_expired_terminals`) → `force_terminal =
///     true`: routes through `reaper::discharge_result`, which FORCES every leg
///     terminal + appends a synthetic CDR, so a deferral caught mid-teardown
///     (`Terminating`, peer-silent) is resolved too.
///
/// Materialise-first so `backup_of` resolves and the propagated delete reaches the
/// peer. The caller MUST hold the per-call lock. No-op if already resident
/// (idempotent reclaim/reap re-pass — `materialize_if_absent` returns false).
pub(super) async fn discharge_materialized_terminal(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    terminal: Call,
    now_ms: i64,
    force_terminal: bool,
) {
    if !ctx.state.materialize_if_absent(terminal.clone(), MaterialiseOrigin::Reclaim) {
        return;
    }
    let mut before = terminal.clone();
    before.state = CallModelState::Active; // synth non-terminal → became_terminated fires
    let discharged = if force_terminal {
        crate::reaper::discharge_result(terminal, now_ms)
    } else {
        crate::effects::HandlerResult::new(terminal)
    };
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        &before,
        crate::rules::invariants::finalize(discharged),
        now_ms,
        // Already-terminal reclaimed body: whoever served it to terminal
        // answered on the wire (or its caller died with the crashed node).
        // Reclaim-discharge stays OFF the SIP wire (ADR-0022 / ADR-0014).
        crate::rules::invariants::UnansweredCaller::Leave,
    );
    process_result(
        ctx,
        call_ref,
        result,
        now_ms,
        &"the discharge of a materialised terminal body",
        None,
    )
    .await;
}

/// The call's `(p,b)` version vector rendered for a lifecycle line
/// (`-` when the call carries no topology and is therefore non-replicable).
pub(super) fn format_pb(call: &Call) -> String {
    match call.topology.as_ref() {
        Some(t) => format!("({},{})", t.gen, t.bak_gen),
        None => "-".to_string(),
    }
}

/// Which side a refused flush came from. The fold is ONE rule in both
/// directions; the direction picks the refusal counter and labels the line.
#[derive(Clone, Copy, Debug)]
pub(super) enum FlushDirection {
    /// An acting backup's flush toward the primary that owns the call.
    Reverse,
    /// A primary's flush toward a backup that holds its Element (ADR-0031 D3).
    Forward,
}

impl FlushDirection {
    /// The label an operator correlates the fold line by.
    fn label(self) -> &'static str {
        match self {
            FlushDirection::Reverse => "reverse",
            FlushDirection::Forward => "forward",
        }
    }

    /// The role this node must play for the ref for a flush of this direction to
    /// be meaningful: a Reverse flush reaches the ref's PRIMARY, a Forward flush
    /// its backup. A command that names the other role is a misrouted frame and
    /// the fold does nothing with it.
    fn expected_role(self) -> PartitionRole {
        match self {
            FlushDirection::Reverse => PartitionRole::Primary,
            FlushDirection::Forward => PartitionRole::Backup,
        }
    }

    /// Whether this direction lands in the call's record: a Reverse flush
    /// reaches the primary's live copy, whose body is the primary's and so
    /// names no version adopted without its body
    /// ([`CallTopology::gen_adopted`](call::CallTopology::gen_adopted)).
    pub(super) fn into_the_record(self) -> bool {
        matches!(self, FlushDirection::Reverse)
    }

    /// Count a fold refusal. The forward direction is already counted by op
    /// where the store refused it (`b2bua_repl_forward_flush_refused_total`), so
    /// only the reverse direction — whose sole refusal site is this fold —
    /// counts here.
    fn count_refused(self, metrics: &crate::metrics::B2buaMetrics) {
        if let FlushDirection::Reverse = self {
            metrics.bump_repl_reverse_flush_refused();
        }
    }
}

/// Fold a flush the store's `(p,b)` gate refused (the body rides the command,
/// the store never took it): the call model reads it for lifecycle progress
/// ([`lifecycle_progress`]) our live copy lacks — the other owner's answer or
/// teardown of a call we read one step behind, folded whole as an applied flush
/// would be, or the end of a re-INVITE round we hold unfinished or the opening
/// of one we never reached, whose facts alone are folded. Anything else stays
/// refused. **A live copy is required**: an
/// Element alone is kept as it is, its merge deferred to the next
/// materialisation, and a call we do not serve live is never resurrected here.
pub(super) async fn fold_refused_flush(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    body: &[u8],
    origin_now_ms: i64,
) {
    if role_of(&ctx.config.self_ordinal, call_ref) != direction.expected_role() {
        return;
    }
    let Some(replica) = ctx.state.decode_body(body) else {
        return;
    };
    // No live copy ⇒ nothing to fold — tested BEFORE the per-call lock, because
    // locking a call this node does not serve inserts a `locks` entry only a
    // release clears, and an Element with no live copy is the common case.
    if ctx.state.peek(call_ref).is_none() {
        return;
    }
    let guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    let Some(live) = ctx.state.peek(call_ref) else {
        // The call was released between the peek and the lock. Drop the guard
        // FIRST: `discard_orphan` removes the very `locks` entry this guard is
        // held on, and a live guard would re-strand it.
        drop(guard);
        ctx.state.discard_orphan(call_ref);
        return;
    };
    // Another call on the ref (the one the live call retried, or one a
    // takeover copy still serves) is never this call's progress.
    if replica.incarnation() != live.incarnation() {
        return;
    }
    let Some(progress) = lifecycle_progress(&replica, &live) else {
        direction.count_refused(&ctx.metrics);
        adopt_seen_counter(ctx, direction, &live, &replica);
        return;
    };
    let skew_offset_ms = if origin_now_ms > 0 { now_ms - origin_now_ms } else { 0 };
    // Whose ending signals end the call is the direction: toward a PRIMARY the
    // live copy defers to the record it owns, so a folded terminal is discharged
    // (Model Y, ADR-0014 §2 / ADR-0020 X3); toward a BACKUP the live takeover
    // copy IS the record and only its own signals end it, so an ending body is
    // read for its counter alone and the call plays on.
    let ends_the_call =
        matches!(direction, FlushDirection::Reverse) && replica.state == CallModelState::Terminated;
    if !ends_the_call && replica.state != CallModelState::Active {
        adopt_seen_counter(ctx, direction, &live, &replica);
        return;
    }
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %live.a_leg.call_id,
        direction = direction.label(),
        state = ?replica.state,
        replica_pb = %format_pb(&replica),
        live_pb = %format_pb(&live),
        "refused flush folded (lifecycle progress over the vector)"
    );
    if ends_the_call {
        // The discharge writes and propagates from this view: it must name the
        // version the other owner reached, or the next flush is refused in turn.
        adopt_seen_counter(ctx, direction, &live, &replica);
        discharge_folded_terminal(ctx, call_ref, &live, replica, now_ms).await;
        return;
    }
    fold_quietly(
        ctx,
        direction,
        call_ref,
        &live,
        replica,
        Fold::of(progress),
        skew_offset_ms,
        now_ms,
    )
    .await;
}

/// A REFUSED flush still moves the counter: seeing a version is not taking it,
/// but this node's next flush must dominate that version or the two views refuse
/// each other for ever (ADR-0031 D3). The live copy keeps its own content and
/// takes only the axis the *other* owner bumps — `b` at a primary, `p` at a
/// backup — through [`adopt_version`](crate::store::CallState::adopt_version),
/// which records it without the authoritative bump an ordinary mutation makes.
/// It takes the higher dialog sequences too ([`raise_sequences`]): the other
/// owner's requests left on the wire, and this node's next request on each
/// dialog continues from them (RFC 3261 §12.2.1.1). A round the refused body
/// holds stays one this node never reached: rounds are read from their
/// transactions, not from the sequence (`call::helpers::round_opened`).
///
/// The adopted view is **flushed**, not only written to the live map: an
/// adoption that never reaches the store is inert — it is lost if a takeover
/// copy self-releases before the next local mutation, and until then this node's
/// stored `(p,b)` still names the version the other owner refuses, so the split
/// stays open and its next flush is refused in turn.
///
/// At a backup the flush never lands over the version it adopted
/// ([`flush_adopted`](crate::store::CallState::flush_adopted)): the primary may
/// have refused the copy's write, adopted its `b` and republished its own call
/// at that very version, which the Element already took — the copy's body is
/// not to replace it. At a primary the adopted flush is its own call over the
/// backup's body its store took at that version, and lands.
fn adopt_seen_counter(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    live: &Call,
    replica: &Call,
) {
    let (Some(l), Some(r)) = (live.topology.as_ref(), replica.topology.as_ref()) else {
        return;
    };
    let raised = raise_sequences(live.clone(), replica);
    if raised != *live {
        ctx.state.update_quiet(raised);
    }
    let (gen, bak_gen) = match direction {
        FlushDirection::Reverse => (l.gen, r.bak_gen),
        FlushDirection::Forward => (r.gen, l.bak_gen),
    };
    let Some(adopted) = ctx.state.adopt_version(&live.call_ref, gen, bak_gen) else { return };
    match direction {
        FlushDirection::Reverse => ctx.state.flush(&adopted),
        FlushDirection::Forward => ctx.state.flush_adopted(&adopted),
    }
}

/// The ADR-0014 **Reverse** apply rule for a live-map fold, and how the
/// reverse-flushed `replica` folds into our `live` copy. It folds whole when
/// the `(p,b)` vector says it is newer — `p` unchanged since the backup
/// branched, `b` advanced, and the backup's body holds the version our `p`
/// names — or when it carries lifecycle progress a reclaimed copy cannot have
/// made on its own: a `p` bump from a turn on a stale record does not outrank
/// the backup's answer or teardown. It folds only its rounds when its sole
/// progress is a re-INVITE round's end or opening ([`lifecycle_progress`]):
/// our own writes since the backup branched stay. A `p` the backup adopted
/// without the body that carried it (ADR-0031 D3,
/// [`CallTopology::gen_adopted`](call::CallTopology::gen_adopted)) never makes
/// its body newer, and neither does a body behind ours ([`lifecycle_behind`],
/// the order the forward gate reads too: an unanswered teardown never ends an
/// answered call). A call with no topology never folds.
fn reverse_fold(replica: &Call, live: &Call) -> Option<Fold> {
    let (Some(r), Some(l)) = (replica.topology.as_ref(), live.topology.as_ref()) else {
        return None;
    };
    let newer = r.gen == l.gen
        && !r.gen_adopted
        && r.bak_gen > l.bak_gen
        && !lifecycle_behind(replica, live);
    if newer {
        return Some(Fold::Whole);
    }
    lifecycle_progress(replica, live).map(Fold::of)
}

/// Whether the reverse-flushed `replica` folds into `live` at all
/// ([`reverse_fold`]).
#[cfg(test)]
fn reverse_flush_dominates(replica: &Call, live: &Call) -> bool {
    reverse_fold(replica, live).is_some()
}

/// Force the last persisted snapshot of `call_ref` terminal and run it through
/// the ordinary `finalize → enforce → process_result` funnel: the `ObligationSet`
/// discharges the CDR + limiter holds, `RemoveCall` rides
/// `release_call(Terminated)`, and the delete propagates. Reached ONLY by the
/// reaper `OUTCOME_DISCHARGE` branch — a takeover copy DEFERS its discharge to
/// the primary instead of discharging here (see the `CallQuiesced` handler and
/// `process_result`). `discharge_result` forces every leg terminal with NO wire
/// traffic. The caller MUST hold the per-call lock.
pub(super) async fn discharge_as_own(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) {
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let before = call.clone();
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        &before,
        crate::rules::invariants::finalize(crate::reaper::discharge_result(call, now_ms)),
        now_ms,
        // LIVE call the rules path failed on twice — if its a-leg is still
        // unanswered the caller is waiting on OUR server txn: answer it.
        crate::rules::invariants::UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
    );
    process_result(ctx, call_ref, result, now_ms, &"the reaper discharge", None).await;
}

#[cfg(test)]
mod tests;
