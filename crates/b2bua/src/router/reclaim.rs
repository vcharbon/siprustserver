//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): the reactive reverse-flush reconcile, on-reboot
//! discharge of deferred terminals; the bulk reboot reclaim and the straggler
//! reclaim live in `bulk`, how two copies of a call meet in `meeting` and how
//! two diverged copies end in `diverged` (ADR-0055 §7), the periodic
//! replica-store reap in `reap`.

use std::sync::Arc;

use call::CallModelState;

use super::interpret::process_result;
use super::restore_hygiene::reanchor;
use super::RouterCtx;
use crate::store::{role_of, CallTopology, Enveloped, MaterialiseOrigin, PartitionRole};

mod bulk;
mod diverged;
mod ending;
mod meeting;
mod reap;
pub(super) use bulk::reclaim_all;
use bulk::reclaim_into_live;
pub(super) use meeting::meet_live;
pub(crate) use reap::reap_expired_replicas;

/// Apply a backup's reverse-flushed copy that the puller just landed in our
/// `pri:{self}` partition (ADR-0014 Reclaim-tail reconcile, extended to the
/// live copy). The acting backup served the call while we still own it live:
/// its copy meets our live copy ([`meet_live`], ADR-0055 §7) — Model Y: the
/// live primary is the *sole* discharge authority, the backup only defers.
///
/// Not live: a **Terminated** body is a reboot-reclaimed deferral — materialise +
/// discharge (no keepalive arming); anything else is the existing reactive
/// straggler, materialised + re-armed by [`reclaim_into_live`].
pub(super) async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
    // A read that sees an expired body too: an expired reverse-flushed terminal
    // is still discharged here.
    let Some((Enveloped { call: first, .. }, _)) = ctx.state.peek_reclaimable_raw(call_ref).await
    else {
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
    let Some((mut replica, skew_offset_ms)) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        if ctx.state.peek(call_ref).is_none() {
            drop(guard);
            ctx.state.discard_orphan(call_ref);
        }
        return;
    };
    reanchor(&mut replica.call, skew_offset_ms);
    let now_ms = ctx.clock.now_ms();
    match ctx.state.peek_enveloped(call_ref) {
        // The stored body is another call than the live one (the release of
        // the stored call is still queued): it is no copy of this call.
        Some(live) if replica.call.incarnation() != live.call.incarnation() => {}
        Some(live) => {
            meet_live(ctx, FlushDirection::Reverse, call_ref, &live, replica, now_ms).await;
        }
        // A reverse-flushed deferral for a call we no longer hold live (the
        // reactive straggler equivalent of the reboot-reclaim terminal): discharge.
        None => {
            if replica.call.state == CallModelState::Terminated {
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
/// Returns whether it discharged the body.
pub(super) async fn discharge_materialized_terminal(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    terminal: Enveloped,
    now_ms: i64,
    force_terminal: bool,
) -> bool {
    if !ctx.state.materialize_if_absent(terminal.clone(), MaterialiseOrigin::Reclaim) {
        return false;
    }
    let terminal = terminal.call;
    let mut before = terminal.clone();
    before.state = CallModelState::Active; // synth non-terminal → became_terminated fires
    let discharged = if force_terminal {
        crate::reaper::discharge_result(terminal, now_ms)
    } else {
        crate::effects::HandlerResult::new(terminal)
    };
    let mut notes = crate::turn_notes::TurnNotes::default();
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        &before,
        crate::rules::invariants::finalize(discharged),
        now_ms,
        // Already-terminal reclaimed body: whoever served it to terminal
        // answered on the wire (or its caller died with the crashed node).
        // Reclaim-discharge stays OFF the SIP wire (ADR-0022 / ADR-0014).
        crate::rules::invariants::UnansweredCaller::Leave,
        &mut notes,
    );
    super::notes::drain_notes(ctx, call_ref, notes);
    process_result(
        ctx,
        call_ref,
        result,
        now_ms,
        &"the discharge of a materialised terminal body",
        None,
    )
    .await;
    true
}

/// A call's `(p,b)` version vector rendered for a lifecycle line (`-` when
/// the call is placed under no topology and is therefore non-replicable).
pub(super) fn format_pb(topology: Option<&CallTopology>) -> String {
    match topology {
        Some(t) => format!("({},{})", t.gen, t.bak_gen),
        None => "-".to_string(),
    }
}

/// Whose copy of the call meets the live copy here. The meeting is ONE rule
/// in both directions; the direction says which copy is the primary's.
#[derive(Clone, Copy, Debug)]
pub(super) enum FlushDirection {
    /// An acting backup's flush toward the primary that owns the call.
    Reverse,
    /// A primary's flush toward a backup that holds its Element (ADR-0031 D3).
    Forward,
}

impl FlushDirection {
    /// The label an operator correlates the meeting line by.
    pub(super) fn label(self) -> &'static str {
        match self {
            FlushDirection::Reverse => "reverse",
            FlushDirection::Forward => "forward",
        }
    }

    /// The role this node must play for the ref for a flush of this direction to
    /// be meaningful: a Reverse flush reaches the ref's PRIMARY, a Forward flush
    /// its backup. A command that names the other role is a misrouted frame and
    /// the meeting does nothing with it.
    fn expected_role(self) -> PartitionRole {
        match self {
            FlushDirection::Reverse => PartitionRole::Primary,
            FlushDirection::Forward => PartitionRole::Backup,
        }
    }
}

/// Meet a copy the store did not take with the live copy (the body rides the
/// command, the store kept its own): the store's copy contains it, or the two
/// diverged, and the live copy may hold more than the store's
/// ([`meet_live`], ADR-0055 §7). **A live copy is required**: an Element
/// alone is kept as it is, and a call we do not serve live is never
/// resurrected here.
pub(super) async fn meet_refused_flush(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    body: &[u8],
    origin_now_ms: i64,
) {
    if role_of(&ctx.config.self_ordinal, call_ref) != direction.expected_role() {
        return;
    }
    let Some(mut replica) = ctx.state.decode_body(body) else {
        return;
    };
    // No live copy ⇒ nothing to meet — tested BEFORE the per-call lock,
    // because locking a call this node does not serve inserts a `locks` entry
    // only a release clears, and an Element with no live copy is the common
    // case.
    if ctx.state.peek(call_ref).is_none() {
        return;
    }
    let guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    let Some(live) = ctx.state.peek_enveloped(call_ref) else {
        // The call was released between the peek and the lock. Drop the guard
        // FIRST: `discard_orphan` removes the very `locks` entry this guard is
        // held on, and a live guard would re-strand it.
        drop(guard);
        ctx.state.discard_orphan(call_ref);
        return;
    };
    // Another call on the ref (the one the live call retried, or one a
    // takeover copy still serves) is never a copy of this call.
    if replica.call.incarnation() != live.call.incarnation() {
        return;
    }
    reanchor(&mut replica.call, now_ms.saturating_sub(origin_now_ms));
    meet_live(ctx, direction, call_ref, &live, replica, now_ms).await;
}

/// Force the last persisted snapshot of `call_ref` terminal and run it through
/// the ordinary `finalize → enforce → process_result` funnel: the `ObligationSet`
/// discharges the CDR + limiter holds, `RemoveCall` rides
/// `release_call(Terminated)`, and the delete propagates. Reached ONLY by the
/// reaper `OUTCOME_DISCHARGE` branch — a takeover copy DEFERS its discharge to
/// the primary instead of discharging here (see `process_result`). `discharge_result` forces every leg terminal with NO wire
/// traffic. The caller MUST hold the per-call lock.
pub(super) async fn discharge_as_own(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) {
    let Some(call) = ctx.state.peek(call_ref) else { return };
    let before = call.clone();
    let mut notes = crate::turn_notes::TurnNotes::default();
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        &before,
        crate::rules::invariants::finalize(crate::reaper::discharge_result(call, now_ms)),
        now_ms,
        // LIVE call the rules path failed on twice — if its a-leg is still
        // unanswered the caller is waiting on OUR server txn: answer it.
        crate::rules::invariants::UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
        &mut notes,
    );
    super::notes::drain_notes(ctx, call_ref, notes);
    process_result(ctx, call_ref, result, now_ms, &"the reaper discharge", None).await;
}

#[cfg(test)]
mod tests;
