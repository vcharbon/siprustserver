//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): bulk reboot reclaim, the reactive reverse-flush
//! reconcile, on-reboot discharge of deferred terminals, and the periodic
//! replica-store reap.

use std::sync::Arc;

use call::{Call, CallModelState, LegState, TimerType};

use super::interpret::process_result;
use super::materialise::{materialise, Materialised, Origin, Reason};
use super::restore_hygiene::{reanchor_timers, sanitize_restored_timers, Smoothing};
use super::RouterCtx;
use crate::store::MaterialiseOrigin;

/// Apply a backup's reverse-flushed mutation that the puller just landed in our
/// `pri:{self}` partition into our **live** map (ADR-0014 Reclaim-tail
/// reconcile, extended to the live copy). The acting-backup served an in-dialog
/// request for a call **we still own live**; fold its dominating `(p,b)` view in
/// so our copy converges — Model Y: the live primary is the *sole* discharge
/// authority, the backup only defers:
/// - **Terminated** → discharge as our own through the reaper funnel — one CDR,
///   limiter released, the propagated delete evicts the backup's deferred copy.
/// - **Active** (a re-INVITE/UPDATE the backup re-originated) → fold the new state
///   in, notably the bumped b-leg `local_cseq`, so OUR next request to the peer
///   continues the dialog monotonically (C11: no CSeq split).
/// - **Terminating** → transient teardown-in-progress; wait for the Terminated
///   flush rather than fold a half-state.
///
/// Not live: a **Terminated** body is a reboot-reclaimed deferral — materialise +
/// discharge (no keepalive arming); anything else is the existing reactive
/// straggler, materialised + re-armed by [`reclaim_into_live`].
///
/// The reverse gate ([`reverse_flush_dominates`]) is re-checked under the
/// per-call lock; a primary that mutated the call since the backup branched
/// keeps its own copy unless the flush carries lifecycle progress it could not
/// have made itself. Idempotent: `update` bumps our `p` and the fold takes the
/// progress, so a re-delivered flush no longer dominates.
pub(super) async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
    // Non-evicting read: an expired reverse-flushed terminal must not be destroyed
    // on access — the backup-durable fallback still needs to discharge it (#7).
    let Some((mut replica, skew_offset_ms)) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        return;
    };
    // Not-live + non-terminal is the original reactive-straggler path; it manages
    // its own per-call lock, so route it BEFORE taking the lock here (the guard is
    // not reentrant).
    if ctx.state.peek(call_ref).is_none() && replica.state != CallModelState::Terminated {
        reclaim_into_live(ctx, call_ref, None).await;
        return;
    }
    let _guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    match ctx.state.peek(call_ref) {
        Some(live) => {
            if !reverse_flush_dominates(&replica, &live) {
                ctx.metrics.bump_repl_reverse_flush_refused();
                return;
            }
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
                // it re-originated (the bumped b-leg `local_cseq`, C11). Fold its
                // state in and let the timer service follow the folded ledger. No
                // discharge — the call continues.
                CallModelState::Active => {
                    resync_timers(ctx, call_ref, &live, &mut replica, skew_offset_ms, now_ms).await;
                    ctx.state.update(replica);
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
        false,
    );
    process_result(ctx, call_ref, result, now_ms).await;
}

/// Discharge an already-`Terminated` body (a backup's deferred terminal, folded
/// into our live map or reclaimed on reboot) through the ONE enforcement funnel.
/// `before` is the live (non-terminal) snapshot so `enforce`'s `became_terminated`
/// edge fires — `discharge_result`/`discharge_as_own` cannot be reused here because
/// they synthesise the terminal from a NON-terminal call, whereas this body is
/// already Terminated (the edge would be vacuous and the ObligationSet would never
/// settle the CDR / limiter / RemoveCall). The CDR + limiter release + propagated
/// delete all ride `process_result`, exactly as a primary-served BYE would.
async fn discharge_folded_terminal(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    before: &Call,
    terminal: Call,
    now_ms: i64,
) {
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        before,
        crate::rules::invariants::finalize(crate::effects::HandlerResult::new(terminal)),
        now_ms,
        // Folded already-terminal body: same off-the-wire contract as
        // `discharge_materialized_terminal` above.
        false,
    );
    process_result(ctx, call_ref, result, now_ms).await;
}

/// Periodic replica-store maintenance. **No CDR is ever written here** — the
/// acting-backup terminal contract (ADR-0020 X3) makes the **primary the sole CDR
/// authority**: a backup never discharges (no CDR, no delete propagation), *not even*
/// as a durable fallback. But a deferred terminal whose primary never came back to
/// reclaim it (crashed for good, past the replica TTL = `reboot_budget`) must NOT be
/// left to pin its limiter slot or leak its replica body forever. So this pass, in
/// order:
///   1. for each **expired deferred terminal**, release the call's limiter hold(s)
///      (the body carries `limiter_entries`; this is the SAME decrement the discharge
///      funnel would emit) and count it as a lost-CDR cleanup — the accepted
///      double-failure (primary down AND never returns): limiter freed, memory freed,
///      **CDR lost**.
///   2. `reap_replica` then physically evicts every expired body (the just-released
///      terminals + the missed-delete ghosts) and prunes the resurrection tombstones.
/// A primary that reboots *inside* `reboot_budget` reclaims and discharges first, then
/// its propagated delete evicts the backup's copy — so the reclaim-discharge and this
/// lossy cleanup are mutually exclusive by the TTL boundary (no double limiter
/// release). Spawned as a paced task by `b2bua_core`.
pub(crate) async fn reap_expired_replicas(ctx: &Arc<RouterCtx>, now_ms: i64) {
    for terminal in ctx.state.expired_terminal_fallbacks(now_ms).await {
        // No per-call lock: this is a `bak:` Element the backup self-released (never
        // in the live map), and the backup never reclaims its own backup partition
        // (`reclaim_scan` reads `pri:{self}`), so there is no concurrent writer to
        // serialize against — and taking the lock would leak a `locks` map entry
        // (only `release_call`/`discard_orphan` clear it). The decoded snapshot is
        // all the limiter release needs; `reap_replica` then evicts the body.
        release_orphaned_limiter_holds(ctx, &terminal).await;
        ctx.metrics.bump_repl_terminal_lost();
    }
    // Evict the leftover: the deferred terminals just limiter-released + the
    // non-terminal missed-delete ghosts. Frees the replica memory (no CDR).
    ctx.state.reap_replica(now_ms).await;
}

/// Release the cluster-wide limiter hold(s) a never-reclaimed deferred terminal
/// still owns, WITHOUT writing a CDR or propagating a delete. Mirrors the
/// `LimiterObligations` derivation (skip fail-open admissions) so the decrement
/// matches the increment the primary made on admission exactly once. The backup is
/// the only node that can free this slot once its primary is dead for good.
async fn release_orphaned_limiter_holds(ctx: &Arc<RouterCtx>, call: &Call) {
    let holds = crate::limiter::live_holds(call);
    if !holds.is_empty() {
        ctx.limiter.release(&holds).await;
    }
}

/// The call's `(p,b)` version vector rendered for a lifecycle line
/// (`-` when the call carries no topology and is therefore non-replicable).
pub(super) fn format_pb(call: &Call) -> String {
    match call.topology.as_ref() {
        Some(t) => format!("({},{})", t.gen, t.bak_gen),
        None => "-".to_string(),
    }
}

/// Fold a reverse flush the store's `(p,b)` gate refused (the body rides the
/// command, the store never took it): the call model reads it for lifecycle
/// progress ([`lifecycle_rank`]) our live copy lacks — a backup's answer or
/// teardown of a call we reclaimed as still ringing — and folds exactly as an
/// applied flush would. Anything else stays refused; a call we do not serve
/// live is not resurrected from it.
pub(super) async fn fold_refused_reverse_flush(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    body: &[u8],
    origin_now_ms: i64,
) {
    let Some(mut replica) = ctx.state.decode_body(body) else {
        return;
    };
    let _guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    let Some(live) = ctx.state.peek(call_ref) else {
        return;
    };
    if lifecycle_rank(&replica) <= lifecycle_rank(&live) {
        ctx.metrics.bump_repl_reverse_flush_refused();
        return;
    }
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %live.a_leg.call_id,
        state = ?replica.state,
        replica_pb = %format_pb(&replica),
        live_pb = %format_pb(&live),
        "reverse-flush reconcile (lifecycle progress over the vector)"
    );
    let skew_offset_ms = if origin_now_ms > 0 { now_ms - origin_now_ms } else { 0 };
    match replica.state {
        CallModelState::Terminated => {
            discharge_folded_terminal(ctx, call_ref, &live, replica, now_ms).await;
        }
        CallModelState::Active => {
            resync_timers(ctx, call_ref, &live, &mut replica, skew_offset_ms, now_ms).await;
            ctx.state.update(replica);
        }
        CallModelState::Terminating => {}
    }
}

/// The ADR-0014 **Reverse** apply rule for a live-map fold. The reverse-flushed
/// `replica` dominates our `live` copy when the `(p,b)` vector says so (`p`
/// unchanged since the backup branched, `b` advanced) OR when it carries
/// lifecycle progress ([`lifecycle_rank`]) a reclaimed copy cannot have made on
/// its own: a `p` bump from a turn on a stale record does not outrank the
/// backup's answer or teardown. A call with no topology never folds.
fn reverse_flush_dominates(replica: &Call, live: &Call) -> bool {
    let (Some(r), Some(l)) = (replica.topology.as_ref(), live.topology.as_ref()) else {
        return false;
    };
    (r.gen == l.gen && r.bak_gen > l.bak_gen) || lifecycle_rank(replica) > lifecycle_rank(live)
}

/// Where a call stands on its lifecycle: unanswered, the caller answered,
/// ending, ended. Only forward progress along this order is a fact a backup can
/// carry that the primary lacks.
fn lifecycle_rank(call: &Call) -> u8 {
    match call.state {
        CallModelState::Terminated => 3,
        CallModelState::Terminating => 2,
        CallModelState::Active => u8::from(call.a_leg.state == LegState::Confirmed),
    }
}

/// The live timer service follows the folded ledger: every entry the folded
/// body no longer carries is cancelled, every entry it carries is (re)armed in
/// this node's clock frame through the restore-hygiene seam, and the folded
/// body keeps the re-anchored ledger it was armed from.
async fn resync_timers(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    folded: &mut Call,
    skew_offset_ms: i64,
    now_ms: i64,
) {
    for stale in live.timers.iter().filter(|t| !folded.timers.iter().any(|f| f.id == t.id)) {
        ctx.timers.cancel(call_ref.to_string(), stale.id.clone()).await;
    }
    sanitize_restored_timers(
        &mut folded.timers,
        call_ref,
        now_ms,
        Some(skew_offset_ms),
        ctx.config.keepalive_interval_sec * 1000,
        None,
    );
    ctx.timers.restore(folded.timers.clone(), call_ref.to_string()).await;
}

/// **Bulk reclaim** (ADR-0014): re-materialise every `pri:{self}`
/// call into the live map + re-arm its timers — what makes a rebooted primary
/// re-*serve* its partition, not just re-*store* it. The scan decodes the
/// partition to size the cohort; each body is then materialised through the
/// evicting reclaim read, so a body whose TTL ran out is dead and is evicted,
/// never re-served: it counts in `scanned` and not in `materialized`.
///
/// **Keepalive smoothing (ADR-0014, performance-only).** Many keepalive timers in
/// a just-rehydrated partition are past-due; firing them all at once floods the
/// node with a synchronized OPTIONS burst. So we stagger the past-due keepalives
/// oldest-first: with `L = now - fire_at` the overdue gap and `L_max` the largest
/// over the batch, a keepalive's new `fire_at` is `now + (L_max - L)/speedup`, so
/// the most-overdue (most at-risk of a UAC keepalive timeout) fires first and the
/// backlog drains over `L_max/speedup`, bounded to `speedup`× the normal cadence
/// (optionally capped by `max_catchup_window_sec`). After the burst each call
/// re-arms `+interval`, naturally re-spreading load. This is **load management
/// only** — `(p,b)` reconciliation makes any incidental keepalive overlap
/// non-corrupting, so there is no settle/handback floor. `fire_at` is pre-computed
/// here, in the reclaim handler — never inside the timer driver (CLAUDE.md).
pub(super) async fn reclaim_all(ctx: &Arc<RouterCtx>) {
    let start_ms = ctx.clock.now_ms();
    let now_ms = start_ms;
    let active_before = ctx.state.active_count() as u64;
    let mut calls = ctx.state.reclaim_scan().await;
    let scanned = calls.len() as u64;
    // The cohort classification below (past-due vs future-dated) and `l_max`
    // read SKEW-CORRECTED deadlines: re-anchor every scanned copy by its own
    // persisted offset first. These copies are discarded; `materialise` re-reads
    // each body and re-anchors it once itself.
    for (call, skew) in calls.iter_mut() {
        reanchor_timers(&mut call.timers, *skew);
    }
    // L_max = the largest past-due keepalive gap across the whole partition, over
    // the now skew-corrected deadlines.
    let l_max = calls
        .iter()
        .flat_map(|(c, _)| c.timers.iter())
        .filter(|t| matches!(t.timer_type, TimerType::Keepalive))
        .map(|t| (now_ms - t.fire_at).max(0))
        .max()
        .unwrap_or(0);
    let smoothing = Smoothing {
        now_ms,
        l_max,
        speedup: ctx.config.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.config.max_catchup_window_sec.map(|s| s * 1000),
    };
    let mut materialized = 0u64;
    for (call, _skew) in calls {
        if reclaim_into_live(ctx, &call.call_ref, Some(smoothing)).await {
            materialized += 1;
        }
    }
    // Per-reboot completeness telemetry. The gauges expose the pass's
    // denominator/numerator; the structured lifecycle line (visible in
    // `kubectl logs`) records the per-pass triple.
    ctx.metrics.set_repl_reclaim_pass(scanned, materialized);
    let active_after = ctx.state.active_count() as u64;
    let duration_ms = ctx.clock.now_ms() - start_ms;
    tracing::info!(
        node = observe::node(),
        active_before,
        scanned,
        materialized,
        active_after,
        l_max_ms = l_max,
        duration_ms,
        "reboot reclaim"
    );
}

/// Reclaim one call of this node's `pri:{self}` partition into the live map
/// under its per-call lock (ADR-0011 X11). `smoothing = Some(_)` is the bulk
/// reboot sweep ([`reclaim_all`]); `None` a single straggler. Returns `true`
/// iff this pass did the work — freshly materialised, or a deferred terminal
/// discharged — so the caller meters per-pass reclaim completeness; `false`
/// for a call already resident (idempotent re-pass) or gone from the store.
async fn reclaim_into_live(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    smoothing: Option<Smoothing>,
) -> bool {
    let _guard = ctx.state.lock(call_ref).await;
    matches!(
        materialise(ctx, call_ref, Origin::Reclaim(smoothing)).await,
        Materialised::Served(_) | Materialised::Refused(Reason::Terminated)
    )
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
        true,
    );
    process_result(ctx, call_ref, result, now_ms).await;
}
