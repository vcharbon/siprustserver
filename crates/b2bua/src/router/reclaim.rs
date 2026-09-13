//! Replication-driven reclaim and the terminal-discharge funnels (ADR-0011 X11
//! / ADR-0014 / ADR-0020 X3): bulk reboot reclaim, the reactive reverse-flush
//! reconcile, on-reboot discharge of deferred terminals, and the periodic
//! replica-store reap.

use std::sync::Arc;

use call::{helpers::lifecycle_advances, Call, CallModelState, TimerType};

use super::interpret::process_result;
use super::materialise::{materialise, Materialised, Origin, Reason};
use super::restore_hygiene::{reanchor_timers, sanitize_restored_timers, Smoothing};
use super::RouterCtx;
use crate::store::{role_of, MaterialiseOrigin, PartitionRole};

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
                adopt_seen_counter(ctx, FlushDirection::Reverse, &live, &replica);
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
                    adopt_counters(&mut replica, &live);
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
/// ([`lifecycle_advances`]) our live copy lacks — the other owner's answer or
/// teardown of a call we read one step behind — and folds exactly as an applied
/// flush would. Anything else stays refused. **A live copy is required**: an
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
    let Some(mut replica) = ctx.state.decode_body(body) else {
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
    if !lifecycle_advances(&replica, &live) {
        direction.count_refused(&ctx.metrics);
        adopt_seen_counter(ctx, direction, &live, &replica);
        return;
    }
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
        // The delete this discharge propagates must name the version the other
        // owner reached, or the Element it should evict refuses it in its turn.
        adopt_seen_counter(ctx, direction, &live, &replica);
        discharge_folded_terminal(ctx, call_ref, &live, replica, now_ms).await;
        return;
    }
    adopt_counters(&mut replica, &live);
    resync_timers(ctx, call_ref, &live, &mut replica, skew_offset_ms, now_ms).await;
    ctx.state.update(replica);
}

/// A fold adopts BOTH counters: the folded body takes `max` of the two views on
/// each axis before [`CallState::update`](crate::store::CallState::update) bumps
/// this node's own. The next flush this node sends therefore dominates every
/// version either owner published, so the split vector heals instead of each
/// side refusing the other for ever (ADR-0031 D3). A body without topology is
/// not replicable and is left alone.
fn adopt_counters(folded: &mut Call, live: &Call) {
    if let (Some(f), Some(l)) = (folded.topology.as_mut(), live.topology.as_ref()) {
        f.gen = f.gen.max(l.gen);
        f.bak_gen = f.bak_gen.max(l.bak_gen);
    }
}

/// A REFUSED flush still moves the counter: seeing a version is not taking it,
/// but this node's next flush must dominate that version or the two views refuse
/// each other for ever (ADR-0031 D3). The live copy keeps its own content and
/// takes only the axis the *other* owner bumps — `b` at a primary, `p` at a
/// backup — through [`adopt_version`](crate::store::CallState::adopt_version),
/// which records it without the authoritative bump an ordinary mutation makes.
///
/// The adopted view is **flushed**, not only written to the live map: an
/// adoption that never reaches the store is inert — it is lost if a takeover
/// copy self-releases before the next local mutation, and until then this node's
/// stored `(p,b)` still names the version the other owner refuses, so the split
/// stays open and its next delete is refused in turn.
fn adopt_seen_counter(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    live: &Call,
    replica: &Call,
) {
    let (Some(l), Some(r)) = (live.topology.as_ref(), replica.topology.as_ref()) else {
        return;
    };
    let (gen, bak_gen) = match direction {
        FlushDirection::Reverse => (l.gen, r.bak_gen),
        FlushDirection::Forward => (r.gen, l.bak_gen),
    };
    if let Some(adopted) = ctx.state.adopt_version(&live.call_ref, gen, bak_gen) {
        ctx.state.flush(&adopted);
    }
}

/// The ADR-0014 **Reverse** apply rule for a live-map fold. The reverse-flushed
/// `replica` dominates our `live` copy when the `(p,b)` vector says so (`p`
/// unchanged since the backup branched, `b` advanced) OR when it carries
/// lifecycle progress ([`lifecycle_advances`]) a reclaimed copy cannot have made
/// on its own: a `p` bump from a turn on a stale record does not outrank the
/// backup's answer or teardown. A call with no topology never folds.
fn reverse_flush_dominates(replica: &Call, live: &Call) -> bool {
    let (Some(r), Some(l)) = (replica.topology.as_ref(), live.topology.as_ref()) else {
        return false;
    };
    (r.gen == l.gen && r.bak_gen > l.bak_gen) || lifecycle_advances(replica, live)
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

#[cfg(test)]
mod tests {
    //! Pins the fold predicate ([`reverse_flush_dominates`]) directly — which
    //! bodies carry lifecycle progress a live copy lacks, and which are a branch
    //! off a stale view — and the direction rule of [`fold_refused_flush`]
    //! itself: who may end a call on a folded body (ADR-0031 D3).

    use call::{
        Call, CallBodyCodec, CallModelState, CallTopology, LegDisposition, LegState, MsgpackCodec,
    };

    use super::{fold_refused_flush, reverse_flush_dominates, FlushDirection};
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::router::test_support::{invite, node, src};
    use crate::store::MaterialiseOrigin;

    /// A replicable call at `(p, b)`, owned by `w0` and backed up by `w1`.
    fn base(p: i64, b: i64) -> Call {
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut call = build_initial_call(&invite("w0", "w1", "fold"), src(), &config, 0);
        call.topology =
            Some(CallTopology { pri: "w0".into(), bak: "w1".into(), gen: p, bak_gen: b });
        call
    }

    /// The caller is ringing: no final has left toward her yet.
    fn unanswered(p: i64, b: i64, state: CallModelState) -> Call {
        let mut call = base(p, b);
        call.state = state;
        call.a_leg.state =
            if state == CallModelState::Active { LegState::Early } else { LegState::Terminated };
        call.a_leg.disposition = LegDisposition::Pending;
        call.a_leg.invite_final_sent = (state != CallModelState::Active).then_some(480);
        call
    }

    /// The caller was answered — a durable fact of the body: the 2xx this stack
    /// sent her is recorded on the leg and the pair is bridged, both of which
    /// outlive the leg going `Terminated`.
    fn answered(p: i64, b: i64, state: CallModelState) -> Call {
        let mut call = base(p, b);
        call.state = state;
        call.a_leg.state = if state == CallModelState::Active {
            LegState::Confirmed
        } else {
            LegState::Terminated
        };
        call.a_leg.disposition = LegDisposition::Bridged;
        call.a_leg.invite_final_sent = Some(200);
        call
    }

    /// Lifecycle progress is a CHAIN: the replica folds only when it made every
    /// step the live copy made and at least one more. A teardown of a caller who
    /// was never answered, arriving at a live copy whose caller IS answered, is a
    /// ring-deadline teardown on a stale view — a branch, and refused (ADR-0031
    /// D3). Folding it ends a call the other owner is serving.
    #[test]
    fn a_terminal_body_that_never_answered_does_not_fold_into_an_answered_live_copy() {
        let replica = unanswered(1, 0, CallModelState::Terminated);
        let live = answered(3, 1, CallModelState::Active);
        assert!(
            !reverse_flush_dominates(&replica, &live),
            "an unanswered teardown is a branch off a stale view, not progress",
        );
    }

    /// The same shape one step along the chain: a teardown of a call the replica
    /// ALSO reads as answered is genuine progress and folds.
    #[test]
    fn a_terminal_body_that_answered_folds_into_an_answered_live_copy() {
        let replica = answered(1, 1, CallModelState::Terminated);
        let live = answered(3, 0, CallModelState::Active);
        assert!(
            reverse_flush_dominates(&replica, &live),
            "ending is the second axis: terminated over active, both answered",
        );
    }

    /// The answer itself: the replica answered the caller our live copy still
    /// reads as ringing.
    #[test]
    fn an_answer_folds_into_an_unanswered_live_copy() {
        let replica = answered(1, 1, CallModelState::Active);
        let live = unanswered(3, 0, CallModelState::Active);
        assert!(reverse_flush_dominates(&replica, &live), "answered over unanswered");
    }

    /// And the reverse of that is not progress.
    #[test]
    fn an_unanswered_body_does_not_fold_into_an_answered_live_copy() {
        let replica = unanswered(1, 1, CallModelState::Active);
        let live = answered(2, 1, CallModelState::Active);
        assert!(
            !reverse_flush_dominates(&replica, &live),
            "a ringing view never outranks an answered one; the vector says nothing here",
        );
    }

    /// The vector clause is untouched: `p` unchanged since the backup branched
    /// and `b` advanced still folds, whatever the chain says.
    #[test]
    fn the_vector_clause_still_folds_an_advanced_backup_counter() {
        let replica = answered(1, 2, CallModelState::Active);
        let live = answered(1, 1, CallModelState::Active);
        assert!(reverse_flush_dominates(&replica, &live), "p unchanged, b advanced");
    }

    /// The `(p,b)` of the live copy `n` holds for `call_ref`.
    fn live_pb(n: &crate::router::test_support::Node, call_ref: &str) -> (i64, i64) {
        let t = n.core.router_ctx().state.peek(call_ref).expect("the copy is live").topology;
        let t = t.expect("the copy is replicable");
        (t.gen, t.bak_gen)
    }

    /// **Only a primary discharges.** A primary's terminal body reaching the
    /// BACKUP that took its call over carries real progress — but the live
    /// takeover copy is the call's record and only its own signals end it
    /// (ADR-0014 §2 / ADR-0020 X3). The fold reads the body for its counter
    /// alone: no CDR, no release, the call plays on.
    #[tokio::test(start_paused = true)]
    async fn a_forward_terminal_ends_no_call_at_the_backup_and_its_counter_is_adopted() {
        let n = node("w1").await;
        let ctx = n.core.router_ctx();
        let live = answered(1, 2, CallModelState::Active);
        let call_ref = live.call_ref.clone();
        assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

        // The partitioned primary tore its own copy down and flushed it forward.
        let replica = answered(3, 1, CallModelState::Terminated);
        let body = MsgpackCodec::new().encode(&replica);
        fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
        sip_clock::testkit::settle().await;

        let after = ctx.state.peek(&call_ref).expect("the takeover copy still serves the call");
        assert_eq!(after.state, CallModelState::Active, "no discharge in the Forward direction");
        assert!(
            n.cdr.snapshot().is_empty(),
            "an acting backup writes no CDR for a folded terminal"
        );
        // `p` is taken from the flush; `b` is untouched — adopting a version is a
        // read this node records, not a mutation of the call.
        assert_eq!(live_pb(&n, &call_ref), (3, 2), "the seen `p` is adopted, and only it");
    }

    /// A `Terminating` body ends no call in either direction — it is a teardown
    /// still in flight — but the version it names is still seen, so the counter
    /// is adopted and this node's next flush is not refused in its turn.
    #[tokio::test(start_paused = true)]
    async fn a_forward_terminating_body_only_moves_the_counter() {
        let n = node("w1").await;
        let ctx = n.core.router_ctx();
        let live = answered(1, 2, CallModelState::Active);
        let call_ref = live.call_ref.clone();
        assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

        let replica = answered(4, 1, CallModelState::Terminating);
        let body = MsgpackCodec::new().encode(&replica);
        fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
        sip_clock::testkit::settle().await;

        let after = ctx.state.peek(&call_ref).expect("the copy still serves the call");
        assert_eq!(after.state, CallModelState::Active, "a teardown in flight ends nothing");
        assert_eq!(live_pb(&n, &call_ref), (4, 2), "the seen `p` is adopted, and only it");
    }

    /// The direction must match the role this node plays for the ref: a
    /// Forward-labelled command for a call this node is PRIMARY of is a
    /// misrouted frame and folds nothing.
    #[tokio::test(start_paused = true)]
    async fn a_direction_that_contradicts_this_nodes_role_folds_nothing() {
        let n = node("w0").await; // w0 is the ref's primary, so Forward is wrong.
        let ctx = n.core.router_ctx();
        let live = answered(1, 2, CallModelState::Active);
        let call_ref = live.call_ref.clone();
        assert!(ctx.state.materialize_if_absent(live, MaterialiseOrigin::Reclaim));

        let replica = answered(5, 1, CallModelState::Terminated);
        let body = MsgpackCodec::new().encode(&replica);
        fold_refused_flush(ctx, FlushDirection::Forward, &call_ref, &body, 0).await;
        sip_clock::testkit::settle().await;

        assert_eq!(live_pb(&n, &call_ref), (1, 2), "nothing was read, nothing was written");
        assert!(n.cdr.snapshot().is_empty(), "and nothing was discharged");
    }
}
