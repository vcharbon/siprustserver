//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): bulk reboot reclaim, the reactive reverse-flush
//! reconcile, on-reboot discharge of deferred terminals, and the periodic
//! replica-store reap.

use std::sync::Arc;

use call::{Call, CallModelState, TimerType};

use super::interpret::process_result;
use super::restore_hygiene::{reanchor_timers, sanitize_restored_timers, Smoothing};
use super::RouterCtx;

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
/// The reverse `(p,b)` gate (`p_in == p_cur && b_in > b_cur`) is re-checked under
/// the per-call lock; a primary that mutated the call since the backup branched
/// keeps its own copy — ADR-0014's accepted keepalive-vs-takeover CSeq-drop, not a
/// fold. Idempotent: `update` bumps our `p`, so a re-delivered flush no longer
/// dominates.
pub(super) async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
    // Non-evicting read: an expired reverse-flushed terminal must not be destroyed
    // on access — the backup-durable fallback still needs to discharge it (#7).
    let Some(replica) = ctx.state.peek_reclaimable_raw(call_ref).await else {
        return;
    };
    // Not-live + non-terminal is the original reactive-straggler path; it manages
    // its own per-call lock, so route it BEFORE taking the lock here (the guard is
    // not reentrant).
    if ctx.state.peek(call_ref).is_none() && replica.state != CallModelState::Terminated {
        // Skew offset for this straggler (0 if none persisted); the seam re-anchors
        // its timers before re-arm just like the bulk/reactive paths.
        let skew = ctx.state.skew_offset_ms(call_ref);
        reclaim_into_live(ctx, replica, skew, None).await;
        return;
    }
    let _guard = ctx.state.lock(call_ref).await;
    let now_ms = ctx.clock.now_ms();
    match ctx.state.peek(call_ref) {
        Some(live) => {
            if !reverse_flush_dominates(&replica, &live) {
                return;
            }
            match replica.state {
                // The backup deferred a terminal it served; discharge OUR live copy
                // through the funnel with the live (non-terminal) copy as `before`
                // so the `→ Terminated` edge fires the ObligationSet + RemoveCall.
                CallModelState::Terminated => {
                    discharge_folded_terminal(ctx, call_ref, &live, replica, now_ms).await;
                }
                // A re-INVITE/UPDATE the backup re-originated: fold its state in
                // (the bumped b-leg `local_cseq`) so OUR next request continues the
                // dialog monotonically (C11). No discharge — the call continues.
                CallModelState::Active => {
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
async fn discharge_materialized_terminal(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    terminal: Call,
    now_ms: i64,
    force_terminal: bool,
) {
    if !ctx.state.materialize_if_absent(terminal.clone()) {
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

/// The ADR-0014 **Reverse** `(p,b)` apply rule for a live-map fold: the
/// reverse-flushed `replica` dominates our `live` copy iff the primary counter is
/// unchanged (`p_in == p_cur` — we have not mutated since the backup branched) and
/// the backup counter genuinely advanced (`b_in > b_cur`). A call with no topology
/// is non-replicable and never folds.
fn reverse_flush_dominates(replica: &Call, live: &Call) -> bool {
    match (replica.topology.as_ref(), live.topology.as_ref()) {
        (Some(r), Some(l)) => r.gen == l.gen && r.bak_gen > l.bak_gen,
        _ => false,
    }
}

/// **Bulk reclaim** (ADR-0014): re-materialise every `pri:{self}`
/// call into the live map + re-arm its timers — what makes a rebooted primary
/// re-*serve* its partition, not just re-*store* it.
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
    // Re-anchor EVERY call's timers by its own persisted skew offset FIRST, so the
    // cohort classification below (past-due vs future-dated) and `l_max` are
    // computed over SKEW-CORRECTED deadlines — a reclaimer anchored ahead of the
    // origin would otherwise mis-classify a future cohort as past-due and compress
    // it into the catch-up band. Applied here once; the per-call seam is then
    // invoked with `skew_offset_ms = 0` (already done).
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
        // Offset already applied above → 0 here (no double-correction).
        if reclaim_into_live(ctx, call, 0, Some(smoothing)).await {
            materialized += 1;
        }
    }
    // Per-reboot completeness telemetry. The gauges expose the pass's
    // denominator/numerator; the structured stderr line (visible in
    // `kubectl logs`) records the per-pass triple.
    ctx.metrics.set_repl_reclaim_pass(scanned, materialized);
    let active_after = ctx.state.active_count() as u64;
    let duration_ms = ctx.clock.now_ms() - start_ms;
    eprintln!(
        "b2bua-runner reboot reclaim: active_before={active_before} scanned={scanned} \
         materialized={materialized} active_after={active_after} l_max_ms={l_max} \
         duration_ms={duration_ms}"
    );
}

/// Materialise one reclaimed call into the live map + re-arm its timers (ADR-0011
/// X11). `smoothing = Some(_)` re-spreads keepalives per
/// `restore_hygiene::smooth_keepalives` for the bulk reboot sweep ([`reclaim_all`]):
/// past-due ones oldest-first, future-dated ones de-correlated within
/// `[now, fire_at]`. `None` (a single reactive straggler) restores verbatim — no
/// cohort to smooth. Non-keepalive timers keep their absolute deadline either way.
/// Returns `true` iff this call was freshly materialised into the live map (the
/// caller meters per-pass reclaim completeness); `false` if it was already
/// resident (idempotent re-pass).
async fn reclaim_into_live(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    skew_offset_ms: i64,
    smoothing: Option<Smoothing>,
) -> bool {
    let call_ref = call.call_ref.clone();
    // Hold the per-call state lock across materialise + timer re-arm, exactly as
    // `process` does, so a concurrent dispatcher handler for this call_ref cannot
    // interleave and double-arm.
    let _guard = ctx.state.lock(&call_ref).await;
    // A reclaimed body that is already terminal is a backup's DEFERRED terminal
    // (Model Y): the backup served the BYE/CANCEL while we were down and never
    // discharged it (the primary is the sole discharge authority — ADR-0020 X3).
    // Discharge it now on rehydration — write the CDR, release the limiter, propagate
    // the delete — instead of re-serving a dead dialog (which would arm keepalives on
    // it and leak). BOTH terminal shapes are handled here, which is the whole point
    // of "proper management on re-hydration":
    //   - `Terminated` (C6: the backup got the b-leg BYE 200) → discharge the body's
    //     own real BYE CDR as-is (`force_terminal = false`).
    //   - `Terminating` (C7: the b-leg peer was SILENT, so the backup deferred a
    //     teardown-in-progress before self-releasing) → FORCE every leg terminal +
    //     synthesise the CDR (`force_terminal = true`, via `reaper::discharge_result`),
    //     so the half-torn-down dialog is completed on the primary rather than
    //     re-served as a live call that would re-probe a dead peer.
    if matches!(call.state, CallModelState::Terminated | CallModelState::Terminating) {
        let force_terminal = call.state == CallModelState::Terminating;
        discharge_materialized_terminal(ctx, &call_ref, call, ctx.clock.now_ms(), force_terminal)
            .await;
        return true;
    }
    // Single restore-hygiene seam (clock-skew hardening): re-anchor by the
    // persisted skew offset, drop the stale keepalive-timeout, apply the
    // deep-past-due defensive floor, then (bulk sweep only) cohort-smooth.
    sanitize_restored_timers(
        &mut call.timers,
        &call_ref,
        ctx.clock.now_ms(),
        Some(skew_offset_ms),
        ctx.config.keepalive_interval_sec * 1000,
        smoothing,
    );
    let timers = call.timers.clone();
    if ctx.state.materialize_if_absent(call) {
        ctx.timers.restore(timers, call_ref).await;
        ctx.metrics.bump_repl_reclaimed();
        true
    } else {
        false
    }
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
