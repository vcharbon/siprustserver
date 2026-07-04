//! `SipRouter` — consumes the transaction-layer event stream + the timer fire
//! channel, resolves each event's `callRef` (synchronously), dispatches the
//! handler body to the per-call FIFO, and interprets the typed effects in the
//! fixed order (persist → critical → outbound → soft → buffered). Port of the
//! load-bearing half of `SipRouter.ts` (`routeKey` + `withCall` + `processResult`).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallModelState, Direction, LegState, TimerEntry, TimerType};
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::{is_emergency_request, parse_uri_params};
use sip_message::{serialize, SipMessage};
use sip_txn::{IdGen, TransactionLayer};
use tokio::sync::mpsc;

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::{CallDecisionEngine, CallReferResponse};
use crate::dispatch::PerCallDispatcher;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerEffects,
    HandlerResult, OutboundBody, OutboundTxnMode, SoftBoundedEffect,
};
use crate::event::CallEvent;
use crate::initial_invite::{build_initial_call, handle_initial_invite};
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::obligations::ObligationSet;
use crate::overload::OverloadSignal;
use crate::repl::{Readiness, ReadinessState};
use crate::rules::model::RuleAction;
use crate::rules::{execute_rules, ActionExecutor, RuleCall, RuleContext, RuleDefinition, ServiceDef};
use crate::store::CallState;
use crate::timers::TimerService;

/// Everything a handler body + the interpreter need. Shared via `Arc`.
pub struct RouterCtx {
    pub config: B2buaConfig,
    pub state: CallState,
    pub txn: TransactionLayer,
    pub timers: TimerService,
    pub dispatcher: PerCallDispatcher,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub id_gen: Arc<IdGen>,
    pub clock: Clock,
    /// The composed engine rule list — `flatten(services.rules) ++ core_rules()`
    /// (ADR-0016). Equal to `default_rules()` while no service is registered.
    pub rules: Arc<Vec<RuleDefinition>>,
    /// Registered callflow services (ADR-0016). Their `init` hooks run at call
    /// setup; their rules are already flattened into `rules`. Empty until a
    /// service is retrofitted (slices 7/8).
    pub services: Arc<Vec<ServiceDef>>,
    pub metrics: B2buaMetrics,
    /// The obligation registry (ADR-0020 X7) — what every call owes at release
    /// (the CDR, the limiter decrements), derived from the snapshot by
    /// `invariants::enforce` on each `→ Terminated` transition.
    pub obligations: Arc<ObligationSet>,
    /// Self-reported readiness driving the OPTIONS health responder (S7). The
    /// default/legacy path uses [`Readiness::always_ready`] → always 200.
    pub readiness: Readiness,
    /// Worker-side overload signal stamped on every OPTIONS-200 reply as
    /// `X-Overload: v=1; elu=…; gc=…; adm=…` (migration/08). The front proxy's
    /// ELU-band AIMD (`sip_proxy::load_observer`) consumes it; the EWMAs advance
    /// only while a sampler task drives [`OverloadSignal::sample`].
    pub overload: OverloadSignal,
    /// Re-entrant event sink: fire-and-forget work (the async `/call/refer`
    /// round-trip) folds its result back into the router by sending a
    /// `CallEvent::InternalEvent` here, which `run` consumes via `on_event` —
    /// keeping re-entry single-threaded and out of a non-`Send` async cycle.
    pub reentry_tx: mpsc::UnboundedSender<CallEvent>,
}

/// Replication-driven commands the puller/supervisor inject into the router loop
/// (ADR-0011 X11 / ADR-0014 fail-back). Routed through the same single-threaded
/// `run` loop as SIP events so reclaim never races the per-call handlers.
#[derive(Debug, Clone)]
pub enum ReplCommand {
    /// **Bulk reclaim** — materialise every `pri:{self}` call into the
    /// live map + re-arm timers (a rebooted primary re-*serving* its partition,
    /// not just re-storing it). Fired once the supervisor reports bootstrap-complete.
    /// Keepalive timers are *smoothed* (oldest-overdue first; see
    /// [`reclaim_all`]) so a freshly-rehydrated node is not flooded by a burst of
    /// past-due OPTIONS.
    ReclaimAll,
    /// **Reactive reclaim** of one call a backup just reverse-flushed to us — the
    /// flip-race straggler an acting-backup took over *after* the bulk sweep.
    ReclaimCall(String),
}

/// How an event resolves to a call + the leg it arrived on.
struct Resolution {
    call_ref: Option<String>,
    source_leg_id: String,
    direction: Direction,
    initial_invite: bool,
}

/// Run the router loop over the txn-event + timer-fire channels until both close.
pub async fn run(
    ctx: Arc<RouterCtx>,
    mut txn_rx: mpsc::Receiver<sip_txn::TransactionEvent>,
    mut timer_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut reentry_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut repl_rx: mpsc::UnboundedReceiver<ReplCommand>,
) {
    loop {
        tokio::select! {
            ev = txn_rx.recv() => match ev {
                Some(ev) => on_event(&ctx, CallEvent::from_txn(ev)).await,
                None => break,
            },
            ev = timer_rx.recv() => {
                if let Some(ev) = ev {
                    on_event(&ctx, ev).await;
                }
            },
            ev = reentry_rx.recv() => {
                if let Some(ev) = ev {
                    on_event(&ctx, ev).await;
                }
            },
            cmd = repl_rx.recv() => {
                if let Some(cmd) = cmd {
                    on_repl_command(&ctx, cmd).await;
                }
            },
        }
    }
}

/// Interpret a replication-driven [`ReplCommand`] (ADR-0011 X11 / ADR-0014).
async fn on_repl_command(ctx: &Arc<RouterCtx>, cmd: ReplCommand) {
    match cmd {
        ReplCommand::ReclaimAll => reclaim_all(ctx).await,
        ReplCommand::ReclaimCall(call_ref) => reconcile_reverse_flush(ctx, &call_ref).await,
    }
}

/// Apply a backup's reverse-flushed mutation that the puller just landed in our
/// `pri:{self}` partition into our **live** map (ADR-0014 Reclaim-tail reconcile:
/// "catch a post-partition reverse-flush a live-but-partitioned primary missed",
/// extended from the replica store to the live copy — the gap the
/// `FixCallTerminateOnBackup` matrix exposed). The acting-backup served an
/// in-dialog request for a call **we still own live**; fold its dominating `(p,b)`
/// view in so our copy converges — Model Y: the live primary is the *sole*
/// discharge authority, the backup only defers:
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
async fn reconcile_reverse_flush(ctx: &Arc<RouterCtx>, call_ref: &str) {
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
    use crate::limiter::LimiterHold;
    let holds: Vec<LimiterHold> = call
        .limiter_entries
        .iter()
        .filter(|e| e.increment_succeeded != Some(false))
        .map(|e| LimiterHold {
            limiter_id: e.limiter_id.clone(),
            window: e.origin_window,
        })
        .collect();
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
async fn reclaim_all(ctx: &Arc<RouterCtx>) {
    let start_ms = ctx.clock.now_ms();
    let now_ms = start_ms;
    let active_before = ctx.state.active_count() as u64;
    let mut calls = ctx.state.reclaim_scan().await;
    let scanned = calls.len() as u64;
    // Re-anchor EVERY call's timers by its own persisted skew offset FIRST, so the
    // cohort classification below (past-due vs future-dated) and `l_max` are
    // computed over SKEW-CORRECTED deadlines — a reclaimer anchored ahead of the
    // origin would otherwise mis-classify a future cohort as past-due and compress
    // it into the catch-up band (the 2026-06-12 OPTIONS burst). Applied here once;
    // the per-call seam is then invoked with `skew_offset_ms = 0` (already done).
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
    let mut materialized = 0u64;
    for (call, _skew) in calls {
        // Offset already applied above → 0 here (no double-correction).
        if reclaim_into_live(ctx, call, 0, Some((now_ms, l_max))).await {
            materialized += 1;
        }
    }
    // Per-reboot completeness telemetry (long-call-on-reboot study, 2026-06-06).
    // The gauges expose the pass's denominator/numerator; the structured stderr
    // line (visible in `kubectl logs`) records the per-pass triple that previously
    // had to be reconstructed from cumulative counters + active_calls deltas.
    ctx.metrics.set_repl_reclaim_pass(scanned, materialized);
    let active_after = ctx.state.active_count() as u64;
    let duration_ms = ctx.clock.now_ms() - start_ms;
    eprintln!(
        "b2bua-runner reboot reclaim: active_before={active_before} scanned={scanned} \
         materialized={materialized} active_after={active_after} l_max_ms={l_max} \
         duration_ms={duration_ms}"
    );
}

/// Strip a stale `KeepaliveTimeout` from a timer set hydrated off a replica
/// snapshot (reclaim or reactive takeover).
///
/// A `KeepaliveTimeout` guards an in-flight keepalive OPTIONS *client
/// transaction* — armed when the OPTIONS is sent (`keepalive` rule) and
/// cancelled the instant its 200 lands (`absorb-options-200`). It exists on the
/// wire for only the round-trip, but a flush that catches that window
/// replicates it, so a `bak:`/`pri:` snapshot can carry an *armed*
/// `KeepaliveTimeout`. When that snapshot is hydrated onto a different node
/// (the rebooted primary's reclaim, or the acting-backup's reactive takeover),
/// the client transaction it guarded **died with the crashed node** — its 200
/// can never arrive to cancel it. Worse, its absolute `fire_at` came from the
/// dead node's clock and is typically already **past-due**, so `restore` fires
/// it on the next tick → the `keepalive-timeout` rule BYEs *both* legs of a
/// perfectly healthy long hold (the parked UAC then sees an unexpected BYE =
/// SIPp `unexpected_msg`). The smoothing in [`reclaim_into_live`] only defers
/// `Keepalive`, never `KeepaliveTimeout`, so it does not save this entry.
///
/// The hydrated call re-probes safely on its own schedule: its `Keepalive`
/// timer (smoothed for the bulk sweep, immediate for a single straggler) fires
/// a *fresh* OPTIONS and arms a *fresh* `KeepaliveTimeout` against the live
/// node's clock. So dropping the stale guard loses nothing and removes the
/// spurious teardown. Purely local timer hygiene — no clock/settle, no `(p,b)`
/// interaction (ADR-0014 untouched). Matches the endurance residual: held long
/// calls torn down during the crash→reclaim window while reclaim is "complete".
fn drop_stale_keepalive_timeout(timers: &mut Vec<TimerEntry>) {
    timers.retain(|t| !matches!(t.timer_type, TimerType::KeepaliveTimeout));
}

/// Deterministic per-`callRef` hash (FNV-1a, 64-bit) used to de-correlate a
/// rehydrated keepalive cohort in [`smooth_keepalives`]. Deterministic (no random
/// seed) so the same call always lands in the same slot of its interval — a reboot
/// re-pass is idempotent — yet distinct refs spread uniformly. NOT for security.
fn stable_jitter(call_ref: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in call_ref.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Re-spread a reclaimed call's `Keepalive` deadlines for the bulk reboot sweep
/// ([`reclaim_all`]) — load management only, never correctness (`(p,b)`
/// reconciliation makes any incidental keepalive overlap non-corrupting; ADR-0014).
/// Two cohorts, both de-synchronised so a rehydrated partition does not re-probe in
/// a single burst that overruns the single-task front proxy:
/// - **Past-due** (`fire_at <= now`): oldest-first over `[0, l_max/speedup]`
///   (optionally capped) so the most-overdue (most at risk of a UAC keepalive
///   timeout) re-probes first and the backlog drains bounded to `speedup`× cadence.
/// - **Future-dated** (`fire_at > now`): a *clean* reboot rehydrates ~the whole
///   partition at one instant with deadlines clustered inside one interval, so left
///   untouched they fire as a synchronised burst one cadence later (2026-06-12
///   endurance: ~550 OPTIONS/s vs ~20/s steady at ~4000 dialogs/worker, which
///   throttled INVITE forwarding to ~10% for ~2 min — silent, UAC retransmits
///   absorbed it, so the past-due path never saw it). Spread each call into
///   `[now, fire_at]` by [`stable_jitter`]. This only ever moves a probe EARLIER
///   (an early keepalive is harmless; *delaying* one risks the UAC keepalive
///   timeout — the loss we avoid), so there is still no settle/handback floor.
///
/// Non-`Keepalive` timers keep their absolute deadline.
fn smooth_keepalives(
    timers: &mut [TimerEntry],
    call_ref: &str,
    now_ms: i64,
    l_max: i64,
    speedup: i64,
    cap_ms: Option<i64>,
) {
    for t in timers.iter_mut() {
        if !matches!(t.timer_type, TimerType::Keepalive) {
            continue;
        }
        let l = now_ms - t.fire_at;
        if l > 0 {
            let mut offset = (l_max - l) / speedup;
            if let Some(cap) = cap_ms {
                offset = offset.min(cap);
            }
            t.fire_at = now_ms + offset;
        } else {
            let window = t.fire_at - now_ms;
            if window > 0 {
                t.fire_at = now_ms + (stable_jitter(call_ref) % window as u64) as i64;
            }
        }
    }
}

/// Re-anchor **deadband** (ms): a persisted `skew_offset_ms` whose magnitude is
/// below this is NOT applied — it is dominated by replication transit latency +
/// clock jitter, not a genuine inter-node clock disagreement worth correcting.
/// This is what keeps the re-anchor a *no-op* under the single-clock harness,
/// whose coarse `advance` (100 ms chunks + settles between replication hops)
/// inflates `receiver_now − origin_now` to a few hundred ms of pure latency with
/// ZERO real skew — perturbing a keepalive by that latency breaks the harness's
/// strict SIP-transparency oracle. A real host clock STEP (the endurance-20260630
/// artifact) is interval-sized (≥ the keepalive cadence, hundreds of seconds), so
/// it clears this deadband by orders of magnitude. Correcting only skew that
/// materially exceeds latency is also strictly better in production: sub-second
/// offsets do not meaningfully move a 300 s keepalive / 150 s setup timer.
const REANCHOR_DEADBAND_MS: i64 = 1_000;

/// Add the receive-time wall-clock `skew_offset_ms` to every timer's absolute
/// `fire_at` (clock-skew hardening), when the offset clears [`REANCHOR_DEADBAND_MS`].
/// Each replicated `TimerEntry.fire_at` is an epoch-ms deadline minted on the
/// ORIGIN node's clock; the offset (`receiver_now_ms − origin_now_ms`, persisted at
/// replica-put time) shifts it into THIS node's clock frame, so the driver's
/// `fire_at − now_ms` reconstruction bounds restore skew to ~replication latency
/// instead of trusting the dead node's clock unboundedly. A sub-deadband or `0`
/// offset (locally-originated body, negligible skew, or already applied by
/// [`reclaim_all`]) is a no-op. This makes ALL downstream past-due math
/// skew-corrected. No `(p,b)` interaction — accuracy only (ADR-0014 untouched).
fn reanchor_timers(timers: &mut [TimerEntry], skew_offset_ms: i64) {
    if skew_offset_ms.abs() < REANCHOR_DEADBAND_MS {
        return;
    }
    for t in timers.iter_mut() {
        t.fire_at += skew_offset_ms;
    }
}

/// **The single restore-hygiene seam** every failover/reclaim hydration path runs
/// a replicated timer set through before re-arming it into this node's
/// [`TimerService`] (clock-skew hardening). Accuracy/performance only — the
/// `timers.rs` driver stays untouched and `(p,b)`-causal reconciliation remains
/// the sole correctness mechanism (ADR-0014); this introduces NO wall-clock
/// correctness rule, settle window, or handback. In order:
///
/// 1. **Re-anchor** by `skew_offset_ms` ([`reanchor_timers`]) so every deadline is
///    in this node's clock frame — this alone fixes the skew-ahead
///    "OPTIONS-at-takeover" artifact and makes the cohort classification in
///    [`smooth_keepalives`] correct.
/// 2. **Drop stale `KeepaliveTimeout`** ([`drop_stale_keepalive_timeout`]) — the
///    OPTIONS it guarded died with the crashed node.
/// 3. **Defensive floor — ONLY when the offset is UNKNOWN** (`None`: a path that
///    could not re-anchor). A `Keepalive` past-due by ≥ 1× `keepalive_interval` is
///    then treated as uncorrected skew/backlog pathology and re-based to `now +
///    (stable_jitter % interval)` rather than firing an immediate OPTIONS at
///    takeover (which would race the failed-over transaction — the
///    endurance-20260630 artifact). When the offset is KNOWN (`Some`, including a
///    well-synced `0`) it is trusted: a past-due keepalive after correction is a
///    normal catch-up and fires promptly (probe the recovered peer) — this is what
///    keeps the single-clock harness transparent (skew is a known 0, so reclaim
///    keeps the source's OPTIONS timing token-for-token). Deterministic per
///    `(call_ref, timer id)` so a reboot re-pass is idempotent.
/// 4. **Cohort smoothing** ([`smooth_keepalives`]) when `smoothing` is requested
///    (the bulk reboot sweep).
fn sanitize_restored_timers(
    timers: &mut Vec<TimerEntry>,
    call_ref: &str,
    now_ms: i64,
    skew_offset_ms: Option<i64>,
    keepalive_interval_ms: i64,
    smoothing: Option<Smoothing>,
) {
    // 1. Re-anchor into this node's clock frame when the offset is KNOWN.
    if let Some(offset) = skew_offset_ms {
        reanchor_timers(timers, offset);
    }
    // 2. Stale keepalive-timeout hygiene.
    drop_stale_keepalive_timeout(timers);
    // 3. Defensive floor — only for an UNKNOWN offset (see the doc above). Skipped
    //    when the offset is known so a well-anchored past-due keepalive fires
    //    promptly (single-clock transparency).
    if skew_offset_ms.is_none() && keepalive_interval_ms > 0 {
        for t in timers.iter_mut() {
            if !matches!(t.timer_type, TimerType::Keepalive) {
                continue;
            }
            if now_ms - t.fire_at >= keepalive_interval_ms {
                let jitter = (stable_jitter(call_ref) ^ stable_jitter(&t.id))
                    % keepalive_interval_ms as u64;
                t.fire_at = now_ms + jitter as i64;
            }
        }
    }
    // 4. Cohort smoothing (bulk reboot sweep only). Runs AFTER re-anchoring so its
    //    past-due/future classification keys on skew-corrected deadlines.
    if let Some(s) = smoothing {
        smooth_keepalives(timers, call_ref, s.now_ms, s.l_max, s.speedup, s.cap_ms);
    }
}

/// Cohort-smoothing parameters for the bulk reboot sweep, passed through the
/// [`sanitize_restored_timers`] seam. `None` (a single reactive straggler /
/// on-demand reclaim) skips smoothing — there is no cohort to de-correlate.
#[derive(Clone, Copy)]
struct Smoothing {
    now_ms: i64,
    l_max: i64,
    speedup: i64,
    cap_ms: Option<i64>,
}

/// Materialise one reclaimed call into the live map + re-arm its timers (ADR-0011
/// X11). `smoothing = Some((now_ms, l_max))` re-spreads keepalives per
/// [`smooth_keepalives`] for the bulk reboot sweep ([`reclaim_all`]): past-due
/// ones oldest-first, future-dated ones de-correlated within `[now, fire_at]`.
/// `None` (a single reactive straggler) restores verbatim — no cohort to smooth.
/// Non-keepalive timers keep their absolute deadline either way.
/// Returns `true` iff this call was freshly materialised into the live map (the
/// caller meters per-pass reclaim completeness); `false` if it was already
/// resident (idempotent re-pass).
async fn reclaim_into_live(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    skew_offset_ms: i64,
    smoothing: Option<(i64, i64)>,
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
    let smoothing = smoothing.map(|(now_ms, l_max)| Smoothing {
        now_ms,
        l_max,
        speedup: ctx.config.keepalive_catchup_speedup.max(1),
        cap_ms: ctx.config.max_catchup_window_sec.map(|s| s * 1000),
    });
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

/// How a call's per-node runtime state is being released. Every path that frees
/// per-call state funnels through [`release_call`] — the ONE teardown executor —
/// so no path can forget a step of the "all per-call state MUST be released at
/// call end" invariant (CLAUDE.md). The three kinds differ ONLY in which side
/// effects they must NOT perform (encoded here, not in comments):
enum ReleaseKind {
    /// Terminated call: evict from map/index/store and **propagate the delete**
    /// to the replica peer (the `RemoveCall` critical effect).
    Terminated,
    /// **Acting-backup self-release** (ADR-0014): shed a reactive takeover copy
    /// once the transaction(s) the backup served for it have all reached a
    /// terminal state. Local-only — **no** store mutation, **no** delete
    /// propagation: the `bak:{primary}` replica and the reverse-flushed deltas
    /// remain, so the call lives on at its reclaiming primary. Replaces the X11
    /// `Deactivate` watermark handback.
    SelfRelease,
    /// Orphan reject: the 481 path hydrated NO call — only the lock entry and
    /// the dispatch queue exist. **No** store mutation (a `remove` would
    /// reverse-propagate a spurious delete), **no** timers/txns were armed.
    Orphan,
}

/// The single per-call teardown executor (see [`ReleaseKind`]). Owns the full
/// release checklist — map/index entry, store propagation, per-call lock,
/// takeover mark, timers (physical `try_remove`, CLAUDE.md), transactions, and
/// the dispatch queue — so the released-at-call-end invariant lives in ONE
/// place instead of three hand-maintained copies (the orphan-lock ratchet and
/// the timer-tombstone CPU climb were both a copy missing one step).
async fn release_call(ctx: &Arc<RouterCtx>, call_ref: &str, kind: ReleaseKind) {
    match kind {
        ReleaseKind::Terminated => {
            ctx.state.remove(call_ref);
            // Idempotent with an explicit `CancelAllTimers` effect, but no longer
            // dependent on every rule remembering to emit one: a terminated call
            // frees EVERY timer slot it owns now, not at its deadline.
            ctx.timers.cancel_all(call_ref.to_string()).await;
            let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
            // Poison the per-call dispatch queue; its worker exits and bumps
            // `removal` exactly once (dispatch.rs). We deliberately do NOT
            // bump here — removal is counted at the single dispatch-queue
            // teardown site so creations/removals stay a matched pair.
            ctx.dispatcher.enqueue_poison(call_ref);
        }
        ReleaseKind::SelfRelease => {
            if ctx.state.drop_local(call_ref) {
                ctx.timers.cancel_all(call_ref.to_string()).await;
                let _ = ctx.txn.cancel_txns_for_call(call_ref).await;
                ctx.dispatcher.enqueue_poison(call_ref);
                ctx.metrics.bump_repl_self_release();
            }
        }
        ReleaseKind::Orphan => {
            ctx.state.discard_orphan(call_ref);
            ctx.dispatcher.enqueue_poison(call_ref);
        }
    }
}

/// Force the last persisted snapshot of `call_ref` terminal and run it through
/// the ordinary `finalize → enforce → process_result` funnel: the `ObligationSet`
/// discharges the CDR + limiter holds, `RemoveCall` rides
/// `release_call(Terminated)`, and the delete propagates. Now reached ONLY by the
/// reaper `OUTCOME_DISCHARGE` branch — Model Y removed the terminal self-release
/// caller (a takeover copy now DEFERS its discharge to the primary instead of
/// discharging here; see the `CallQuiesced` handler and `process_result`).
/// `discharge_result` forces every leg terminal with NO wire traffic. The caller
/// MUST hold the per-call lock.
async fn discharge_as_own(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) {
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

async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent) {
    // Per-method / per-(method,code) data-path counters. Every inbound SIP
    // message lands here once, so this is the single chokepoint to meter them.
    if let CallEvent::Sip { message, .. } = &event {
        match message.as_ref() {
            SipMessage::Request(req) => ctx.metrics.record_request(req.method.as_str()),
            SipMessage::Response(resp) => ctx.metrics.record_response(resp.cseq.method.as_str(), resp.status),
        }
    }

    // Per-peer timeout attribution (observability only;
    // b2bua_peer_failures_total{peer,scope,kind}). A client transaction gave up
    // with no final response: split response_timeout (Timer B/F) vs
    // transaction_timeout (the long out-of-dialog INVITE backstop) by the
    // forwarded `timeout_kind`, and classify the peer internal/external against
    // the configured outbound proxy. `destination == None` (legacy txns) is
    // skipped rather than fabricated.
    if let CallEvent::Timeout { destination: Some(dest), timeout_kind, .. } = &event {
        let kind = match timeout_kind {
            sip_txn::TimeoutKind::Response => crate::peer_failures::PeerFailureKind::ResponseTimeout,
            sip_txn::TimeoutKind::Transaction => {
                crate::peer_failures::PeerFailureKind::TransactionTimeout
            }
        };
        ctx.metrics.record_peer_failure(dest, classify_b2bua_peer(&ctx.config, dest), kind);
    }

    // ADR-0014 acting-backup self-release. The txn layer reports the last
    // transaction we served for a takeover copy has cleared; shed the live copy
    // (the `bak:` replica + reverse-flushed deltas remain). The per-call lock
    // serializes this against any in-flight handler for the call; we re-check under
    // it because a fresh in-dialog request could have re-armed a transaction (a
    // second takeover during a sustained partition) since the notice was emitted.
    //
    // The guard is held ACROSS the release — never dropped between the re-check
    // and `drop_local`. Dropping it first opened a thread-skew window (multi-
    // threaded runtime only; paused-clock tests never tripped it) where a queued
    // per-call handler could hydrate the still-resident copy (`fresh == false` →
    // no re-mark, no re-watch) and its `process_result → update()` re-inserted a
    // ZOMBIE after `drop_local`: resident, unmarked, unwatched, timers racing-
    // cancelled — the X11 double-serve class ADR-0014 exists to kill. Under the
    // guard, a parked handler acquires only after the call is gone; it then
    // re-hydrates FRESH (re-marked, re-watched) and converges via the next
    // CallQuiesced. `release_call` takes no per-call lock itself, so holding the
    // guard across it is deadlock-free.
    if let CallEvent::CallQuiesced { call_ref } = &event {
        let call_ref = call_ref.clone();
        if ctx.state.is_takeover(&call_ref) {
            let _guard = ctx.state.lock(&call_ref).await;
            if ctx.state.is_takeover(&call_ref) {
                if ctx.txn.active_txn_count_for_call(&call_ref).await.unwrap_or(0) == 0 {
                    // Model Y (ADR-0020 X3 amended): a takeover copy DEFERS its
                    // discharge to the live primary regardless of its state — it is
                    // never an independent CDR/limiter writer. So self-release
                    // unconditionally (drop the live copy; the `bak:` replica + the
                    // reverse-flushed deltas remain):
                    //   - **Active** → the call continues at the reclaiming primary
                    //     (ADR-0014; the C11 non-terminal guard rail).
                    //   - **Terminating/Terminated** → the terminal state was already
                    //     reverse-flushed in `process_result` (the Active|Terminating
                    //     flush gate; a Terminated copy was already deferred there).
                    //     The primary discharges it EXACTLY ONCE — immediately if it
                    //     is alive and reconciling, or on reboot via reclaim. If the
                    //     primary never returns inside the replica TTL the deferral is
                    //     silently evicted, CDR/limiter lost (accepted double-failure).
                    //     This replaces a2dcf4c's discharge-here: the backup
                    //     discharging too was the C7 double-CDR. The "stale Active
                    //     replica re-terminates" storm a2dcf4c fixed is gone a
                    //     different way — the replica is reverse-flushed
                    //     Terminating/Terminated, never Active.
                    if let Some(call) = ctx.state.peek(&call_ref) {
                        if matches!(call.state, CallModelState::Terminating | CallModelState::Terminated) {
                            // Belt-and-braces reverse-flush of the terminal state (a
                            // Terminated copy skips the process_result flush gate) so
                            // the primary's reconcile/reclaim has it — held with the
                            // normal replica TTL (`reboot_budget`) so a rebooting
                            // primary has its full window to reclaim and discharge.
                            ctx.state.flush(&call);
                        }
                    }
                    release_call(ctx, &call_ref, ReleaseKind::SelfRelease).await;
                } else {
                    // A fresh in-dialog request (a second takeover during a
                    // sustained partition) re-armed a transaction since this notice
                    // was emitted, and the txn layer's watch is one-shot — it was
                    // consumed delivering THIS CallQuiesced. Re-arm it so the
                    // eventual last-txn clear notifies us again; otherwise the
                    // takeover copy is stranded double-serving until its 1 h
                    // GlobalDuration backstop (the watch is never re-armed elsewhere
                    // for an already-resident copy: hydrate returns fresh == false).
                    let _ = ctx.txn.watch_self_release(&call_ref).await;
                }
            }
        }
        return;
    }

    // Out-of-dialog OPTIONS keepalive: self-report readiness (S7, ADR-0011 X6).
    // The front proxy probe keys on the status + Reason header text
    // (`sip-proxy::health::probe::classify_503`).
    if let CallEvent::Sip { message, src } = &event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method == "OPTIONS" && req.to.tag.is_none() {
                let resp =
                    build_options_health_response(&ctx.readiness, &ctx.overload, &ctx.id_gen, req);
                let _ = ctx.txn.send_response(resp, *src).await;
                return;
            }
        }
    }

    let mut res = resolve(ctx, &event);
    if res.call_ref.is_none() {
        // Acting-backup takeover BACKSTOP. The normal in-dialog key is the R-URI
        // `callref` param the B2BUA Contact stamps and the proxy preserves under
        // loose routing — so `resolve` (above) already keys the dialog from it,
        // and sip-txn `extract_ruri_call_ref` attributes the server txn by the
        // SAME key (the self-release count gate, ADR-0014). This branch only fires
        // when that param is absent AND our in-memory `sip_index` is empty — a
        // pure backup that never primary-served the call. Re-key the dialog from
        // the replica store's SIP index (the puller imported it) before declaring
        // the event unroutable, so a failed-over in-dialog request is not silently
        // dropped and the dialog can still terminate on the backup.
        res.call_ref = replica_takeover_call_ref(ctx, &event).await;
    }
    let call_ref = match res.call_ref.clone() {
        Some(r) => r,
        None => {
            ctx.metrics.bump_unroutable_dropped();
            return;
        }
    };

    // ── Full-guarantee cap shed (ADR-0022) ────────────────────────────────────
    // At the per-call global cap, `dispatch` would SILENTLY drop a brand-new
    // call_ref's body before any call/txn context exists — leaving a caller who
    // already heard sip-txn's auto-100 on "100-then-silence" (the one full-queue
    // path neither the decision deadline nor the terminated-unanswered synthesis
    // can reach, because no call is ever born). Shed a NEW initial INVITE here
    // with a stateless 503 instead (mirrors the Tier-3 admission gate: stateless,
    // no per-call resources, sent through the INVITE server txn that carries the
    // 100). In-dialog events for an at-cap new call_ref stay on the silent
    // `dispatch` cap-drop (an in-dialog request with no live call is an orphan the
    // protocol resends / the peer 481s; only the initial INVITE owes a final).
    if res.initial_invite && ctx.dispatcher.would_drop_new_at_cap(&call_ref) {
        if let CallEvent::Sip { message, src } = &event {
            if let SipMessage::Request(req) = message.as_ref() {
                let resp = build_stateless_overload_503(
                    &ctx.id_gen,
                    req,
                    ctx.config.retry_after_base_sec,
                );
                let _ = ctx.txn.send_response(resp, *src).await;
            }
        }
        // Count it on the same cap counter (the cap WAS reached); the caller now
        // gets a 503 rather than silence.
        ctx.metrics.bump_cap_drop();
        return;
    }

    let ctx2 = ctx.clone();
    ctx.dispatcher.dispatch(
        &call_ref,
        Box::pin(async move {
            process(&ctx2, event, res).await;
        }),
    );
}

/// Build the self-reported readiness reply to an out-of-dialog OPTIONS
/// keepalive (S7). Every reply mints a local To-tag: RFC 3261 §8.2.6.2 requires
/// a To-tag on any response > 100 to an out-of-dialog request (the 2xx path
/// always did; the 503 path needs it too, and `hydrate_response` rejects a
/// tagless response otherwise). The status + `Reason` header text is the
/// contract `sip-proxy::health::probe::classify_503` keys on:
///   - `Ready`    → `200 OK` + `X-Overload: v=1; elu=…; gc=…; adm=…`.
///   - `NotReady` → `503` + `Reason: SIP;cause=503;text="not-ready"`.
///   - `Draining` → `503` + `Reason: SIP;cause=503;text="draining"` +
///     `Retry-After: 0`.
///
/// The `X-Overload` worker load signal (migration/08) rides the **200 path
/// only**: it is the live signal the proxy's ELU-band AIMD
/// (`sip_proxy::load_observer::parse_x_overload_header`) consumes to steer (and,
/// at `AboveCritical`, exclude) a *serving* worker. A 503 already removes the
/// node from new-dialog selection, so stamping the band signal there is moot.
///
/// **Deliberate divergence from TS (tracked, not an omission):** `SipRouter.ts`
/// also stamps `X-Overload` on the boot-drain 503, and the Rust proxy consumer
/// (`probe.rs`) parses the header off *any* status — so on a draining worker the
/// LB forgoes one fresh ELU sample per probe. We omit it because a draining node
/// is excluded from new-dialog selection anyway and the consumer has no
/// `noteRejectionPayload` fast-path (the AIMD rate-cap is deferred, ADR-0009).
/// Revisit (and revisit `options_200_stamps_x_overload_503_does_not`, which pins
/// this divergent behaviour) when the rate-cap consumer item is ported. See the
/// `MIGRATION_STATUS.md` overload row for the full carry-forward list.
pub(crate) fn build_options_health_response(
    readiness: &Readiness,
    overload: &OverloadSignal,
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
) -> sip_message::SipResponse {
    use sip_message::types::SipHeader;

    let hdr = |name: &str, value: &str| SipHeader {
        name: name.to_string(),
        value: value.to_string(),
    };

    let (status, reason, extra_headers): (u16, &str, Vec<SipHeader>) = match readiness.state() {
        ReadinessState::Ready => (
            200,
            "OK",
            // RFC 3261 §11.2: an OPTIONS 200 SHOULD advertise capabilities so the
            // querier learns method/extension/body support, not just liveness.
            // Plus the worker load signal the proxy's AIMD band reads.
            vec![
                hdr("Allow", sip_message::generators::B2BUA_ALLOW),
                hdr("Accept", "application/sdp"),
                hdr("Supported", sip_message::generators::B2BUA_SUPPORTED),
                hdr("X-Overload", &overload.x_overload_header_value()),
            ],
        ),
        ReadinessState::NotReady => (
            503,
            "Service Unavailable",
            vec![hdr("Reason", "SIP;cause=503;text=\"not-ready\"")],
        ),
        ReadinessState::Draining => (
            503,
            "Service Unavailable",
            vec![
                hdr("Reason", "SIP;cause=503;text=\"draining\""),
                hdr("Retry-After", "0"),
            ],
        ),
    };

    generate_response(
        req,
        status,
        reason,
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers,
            ..Default::default()
        },
    )
}

/// Build the **stateless 503** the Tier-3 admission gate sends when it rejects a
/// new INVITE (migration/09 — port of `MessageHelpers.buildStatelessReject503Buffer`
/// + the `Reason`/`Retry-After` the TS `TransactionLayer` gate stamps).
///
/// Stateless because no server transaction (and no call) is created — the router
/// sends this via [`TransactionLayer::send_raw`](sip_txn::TransactionLayer::send_raw)
/// and returns before `build_initial_call`. It echoes the INVITE's Via/From/To/
/// Call-ID/CSeq (via [`generate_response`]) and adds:
///   - `Reason: SIP;cause=503;text="overload"` — the overload cause token, matching
///     the TS reject buffer (distinct from the readiness 503's `not-ready` / `draining`).
///   - `Retry-After: <retry_after_sec>` — the gate's hint (bucket time-to-token for
///     `bucket_empty`, the configured base for `panic_elu`).
///
/// **One faithful divergence from the TS raw buffer:** the TS builder deliberately
/// omits a To-tag (the UAC's ACK is then orphaned and dropped). This codebase
/// enforces a To-tag on every non-100 final (RFC 3261 §8.2.6.2; `generate_response`
/// adds a fallback, the RFC audit gate flags a tagless final, and the sibling
/// `reject_call` / readiness-503 paths both tag) — so this stamps a fresh tag too.
/// It stays stateless regardless: with no server txn the ACK still can't match a
/// dialog and is dropped at the orphan-ACK path, exactly the cheap-rejection contract.
fn build_stateless_overload_503(
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    retry_after_sec: u32,
) -> sip_message::SipResponse {
    use sip_message::types::SipHeader;
    let hdr = |name: &str, value: String| SipHeader { name: name.to_string(), value };
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers: vec![
                hdr("Reason", "SIP;cause=503;text=\"overload\"".to_string()),
                hdr("Retry-After", retry_after_sec.to_string()),
            ],
            ..Default::default()
        },
    )
}

/// Resolve the `callRef` + source leg for an event (synchronous, no blocking).
fn resolve(ctx: &RouterCtx, event: &CallEvent) -> Resolution {
    match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => {
                if req.method == "INVITE" && req.to.tag.is_none() {
                    let call_ref = call::derive_call_ref(
                        &ctx.config.self_ordinal,
                        &req.call_id,
                        req.from.tag.as_deref().unwrap_or(""),
                    );
                    return Resolution {
                        call_ref: Some(call_ref),
                        source_leg_id: "a".into(),
                        direction: Direction::FromA,
                        initial_invite: true,
                    };
                }
                // In-dialog request: read our cr/lg from the Request-URI params.
                // NB `parse_uri_params` lower-cases param NAMES (URI params are
                // case-insensitive per RFC 3261 §19.1.1), so the stamped `callRef`
                // is keyed as `callref`. The primary path masks a mismatch via the
                // in-memory `sip_index` fallback below; the acting-backup takeover
                // path has no such index, so the param IS the only key — read it by
                // its normalised (lower-case) name.
                let params = parse_uri_params(&req.uri);
                let leg = params
                    .get("leg")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .unwrap_or_else(|| "a".into());
                let call_ref = params
                    .get("callref")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .or_else(|| {
                        ctx.state.resolve_from_sip_key_sync(
                            &req.call_id,
                            req.from.tag.as_deref().unwrap_or(""),
                        )
                    });
                Resolution {
                    direction: leg_direction(ctx, call_ref.as_deref(), &leg),
                    call_ref,
                    source_leg_id: leg,
                    initial_invite: false,
                }
            }
            SipMessage::Response(resp) => {
                // Response: read our cr/lg from the top Via we stamped.
                let (cr, lg) = via_cr_lg(resp.headers.first().map(|h| h.value.as_str()))
                    .or_else(|| {
                        resp.headers
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case("via"))
                            .and_then(|h| via_cr_lg(Some(&h.value)))
                    })
                    .unwrap_or((None, "a".into()));
                let call_ref = cr.or_else(|| {
                    ctx.state.resolve_from_sip_key_sync(&resp.call_id, resp.to.tag.as_deref().unwrap_or(""))
                });
                Resolution {
                    direction: leg_direction(ctx, call_ref.as_deref(), &lg),
                    call_ref,
                    source_leg_id: lg,
                    initial_invite: false,
                }
            }
        },
        CallEvent::Cancelled { call_id, from_tag, .. } => {
            // A CANCEL races the very INVITE it cancels. The initial-INVITE body
            // `create()`s (and indexes) the call on the per-call FIFO *worker*,
            // asynchronously — whereas this `resolve` runs in the run loop the
            // instant the txn layer emits `Cancelled`. So the `sip_index` may not
            // be populated yet, and a sync index miss would drop the CANCEL as
            // unroutable, leaking the b-leg the (still-parked) decision is about
            // to build. DERIVE the callRef the same way the INVITE did
            // (`derive_call_ref(self, callId, fromTag)`) when the index misses, so
            // the CANCEL resolves to the SAME call regardless of create() timing;
            // FIFO ordering then guarantees `handle-cancel` runs after the INVITE
            // body has built the call + b-leg. Deriving with `self_ordinal` is
            // correct here because a CANCEL only targets a brand-new INVITE this
            // node is primary-serving (build_initial_call used the same ordinal);
            // ACK/BYE cannot hit this path — they require an established dialog, so
            // the call (and its index) already exist. A genuinely orphan CANCEL
            // (no INVITE ever) resolves to a callRef with no live call and is
            // reaped cleanly via the orphan path in `process`.
            let call_ref = ctx
                .state
                .resolve_from_sip_key_sync(call_id, from_tag)
                .unwrap_or_else(|| {
                    call::derive_call_ref(&ctx.config.self_ordinal, call_id, from_tag)
                });
            Resolution {
                call_ref: Some(call_ref),
                source_leg_id: "a".into(),
                direction: Direction::FromA,
                initial_invite: false,
            }
        }
        CallEvent::Timeout { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(ctx, call_ref.as_deref(), &leg),
                call_ref: call_ref.clone(),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::Timer { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(ctx, Some(call_ref), &leg),
                call_ref: Some(call_ref.clone()),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::InternalEvent { call_ref, .. } => Resolution {
            call_ref: Some(call_ref.clone()),
            source_leg_id: "a".into(),
            direction: Direction::FromA,
            initial_invite: false,
        },
        // Handled (and returned) in `on_event` before `resolve` is ever called.
        CallEvent::CallQuiesced { .. } => unreachable!("CallQuiesced is handled before resolve"),
    }
}

/// Recover the takeover `callRef` for an in-dialog SIP request from the replica
/// store's SIP index (the acting-backup production path). Only in-dialog requests
/// (those carrying a To-tag) are candidates; an initial request, a response, or a
/// non-SIP event is never a dialog takeover. `None` when not applicable or no
/// replica matches — the caller then treats the event as unroutable.
async fn replica_takeover_call_ref(ctx: &RouterCtx, event: &CallEvent) -> Option<String> {
    let CallEvent::Sip { message, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.to.tag.is_none() {
        return None; // initial request — a brand-new dialog, not a takeover
    }
    ctx.state
        .resolve_from_replica_index(&req.call_id, req.from.tag.as_deref().unwrap_or(""))
        .await
}

fn leg_direction(_ctx: &RouterCtx, _call_ref: Option<&str>, leg: &str) -> Direction {
    if leg == "a" {
        Direction::FromA
    } else {
        Direction::FromB
    }
}

/// Classify a destination as an internal cluster peer or an external one for the
/// per-peer failure metric. A b2bua's only config-resolvable cluster peer is the
/// configured outbound proxy (`b2b_outbound_proxy`): every b-leg egresses through
/// it, so a worker→callee timeout we count against the outbound proxy is the
/// in-cluster hop. Replication-peer addresses are NOT in `B2buaConfig` as
/// `SocketAddr`s (the repl layer addresses peers by endpoint URL, resolved
/// elsewhere), so they fall through to `External` here — a documented limitation;
/// the metric is still bounded and correct, just coarser for repl-peer timeouts
/// (which are rare and would land in the external LRU/overflow).
///
/// The outbound proxy may be configured as a HOSTNAME (not an IP literal). We
/// resolve it via `ToSocketAddrs` (taking the first resolved addr) and compare by
/// resolved IP+port, so the internal pinning fires for a hostname-configured proxy
/// — not just the cluster's VIP IP literal. This path is cold (keepalive/response
/// timeout), so the lazy resolve here is acceptable. If resolution fails, the dest
/// classifies External (fail-open; the metric stays bounded).
/// Resolve the egress-aware next hop of the leg a `KeepaliveTimeout` fired for —
/// the hop the unanswered OPTIONS used — for the per-peer keepalive-timeout metric.
/// Mirrors `bye_on_dialog`'s destination decision (to_gen_dialog → dest_of →
/// `relay::leg_egress_dest`) WITHOUT mutating a request. Returns `None` (record
/// nothing) when the leg or its first dialog can't be resolved — never a fabricated
/// address. The returned `(host, port)` may be a hostname (the outbound proxy); the
/// caller resolves it to a `SocketAddr` before recording.
fn keepalive_timeout_peer(
    config: &B2buaConfig,
    call: &Call,
    leg_id: Option<&str>,
) -> Option<(String, u16)> {
    use crate::rules::relay;
    let leg_id = leg_id?;
    let leg = if leg_id == call.a_leg.leg_id {
        &call.a_leg
    } else {
        call.b_legs.iter().find(|l| l.leg_id == leg_id)?
    };
    let d = leg.dialogs.first()?;
    let gd = relay::to_gen_dialog(&d.sip);
    let base = relay::dest_of(&relay::strip_uri(&gd.remote_target));
    Some(relay::leg_egress_dest(config, leg_id, &gd.route_set, base))
}

fn classify_b2bua_peer(config: &B2buaConfig, dest: &SocketAddr) -> crate::peer_failures::PeerScope {
    use std::net::ToSocketAddrs;
    if let Some((host, port)) = &config.b2b_outbound_proxy {
        if let Ok(mut resolved) = (host.as_str(), *port).to_socket_addrs() {
            if resolved.any(|proxy| proxy == *dest) {
                return crate::peer_failures::PeerScope::Internal;
            }
        }
    }
    crate::peer_failures::PeerScope::External
}

/// The per-call handler body: check the call out, run the handler, interpret.
async fn process(ctx: &Arc<RouterCtx>, event: CallEvent, res: Resolution) {
    let call_ref = res.call_ref.clone().expect("dispatched events carry a callRef");
    let _guard = ctx.state.lock(&call_ref).await;
    let now_ms = ctx.clock.now_ms();

    // ── Call-reaper verdict gate (ADR-0020 X5/X6) — BEFORE any hydration ──
    // A verdict is check-then-act made safe: it applies only if the call's
    // last-touched stamp still matches what the sweep observed (stale) or the
    // call is still resident (fatal-error/discharge). The gate runs before the
    // in-dialog hydrate below so a late verdict for a RELEASED call can never
    // resurrect it from the replica store via on-demand reclaim.
    if let CallEvent::InternalEvent { topic, outcome, payload, .. } = &event {
        if topic == crate::reaper::REAPER_TOPIC {
            let watermark = payload.get("watermark").and_then(|v| v.as_i64());
            let current = ctx.state.last_touched(&call_ref);
            if !crate::reaper::verdict_confirmed(outcome, watermark, current) {
                if current.is_none() && ctx.state.peek(&call_ref).is_none() {
                    // The call is gone — but this verdict's dispatch may have
                    // spun up a fresh per-call queue (+ lock entry). Tear the
                    // ephemera down via the orphan path so nothing ratchets.
                    drop(_guard);
                    release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
                }
                return;
            }
            if outcome == crate::reaper::OUTCOME_DISCHARGE {
                // Strike-2: the rules path itself failed. Force the last
                // persisted snapshot terminal and run it through the ORDINARY
                // finalize → enforce → process_result — the ObligationSet
                // discharges the CDR + limiter holds, RemoveCall rides
                // release_call(Terminated), the delete propagates (X6).
                if ctx.state.peek(&call_ref).is_none() {
                    return;
                }
                ctx.metrics.bump_reaper_discharged();
                discharge_as_own(ctx, &call_ref, now_ms).await;
                return;
            }
            // A confirmed stale / fatal-error verdict falls through to the
            // normal rules (`reaper-stale` / `reaper-fatal-error`). It does
            // NOT refresh the stamp — an ineffective verdict must keep the
            // call stale so the sweep escalates instead of waiting idle_max.
        }
    } else if matches!(event, CallEvent::Sip { .. }) {
        // The last-touched stamp (ADR-0020 X4): liveness derives from **real
        // SIP traffic only** — a received message here, or a turn that sent
        // SIP out (stamped in `process_result` after the outbound effects).
        // Self-generated turns that touch no wire (`LimiterRefresh`, internal
        // events, timer fires whose rules emit nothing) deliberately do NOT
        // stamp: a crash-orphaned call refreshing its limiter holds every
        // 300 s kept itself reaper-"fresh" for a full hour while SIP-dead
        // (endurance 2026-06-12) — the call must not vouch for itself. A
        // wedged FIFO never reaches this line — its stamp freezes, which IS
        // the staleness signal the sweep reads.
        ctx.state.touch(&call_ref, now_ms);
    }

    let result = if res.initial_invite {
        if ctx.state.peek(&call_ref).is_some() {
            return; // retransmitted INVITE for an existing call — ignore
        }
        let (req, src) = match &event {
            CallEvent::Sip { message, src } => match message.as_ref() {
                SipMessage::Request(r) => (r.clone(), *src),
                _ => return,
            },
            _ => return,
        };

        // ── Tier-3 admission gate (migration/09 — port of the `overload.shouldAdmit`
        // + stateless-503 gate in `TransactionLayer.ts`). Only an *initial* INVITE
        // reaches here (`res.initial_invite`); re-INVITEs (To-tag present) and
        // non-INVITE in-dialog requests take the other branch and are never gated,
        // exactly as in the TS source.
        //
        // Faithful split note vs TS: the TS gate runs in the txn layer *before* the
        // server txn / 100 Trying. In the Rust split, sip-txn already created the
        // INVITE server txn and auto-sent 100 Trying before emitting this Message
        // (and the b2bua's `OverloadController`/config live a crate above sip-txn —
        // the ADR-0007 deferral the layer.rs NOTE records). So the reject is sent
        // *through that server txn* (`send_response`, which supersedes the cached
        // 100 and drives the txn → Completed with proper retransmission + ACK
        // absorption) rather than as a wire-raw datagram. It is still **stateless at
        // the call layer** — no `build_initial_call`/`create`, so no dialog, CDR,
        // limiter hold, or replicated state is ever born; we `return` before any of
        // it. That "no per-call resources for a rejected INVITE" is the property the
        // Tier-3 gate is about.
        let is_emergency = is_emergency_request(&req);
        let decision = ctx.overload.should_admit(is_emergency);
        if !decision.admit {
            let resp = build_stateless_overload_503(&ctx.id_gen, &req, decision.retry_after_sec);
            let _ = ctx.txn.send_response(resp, src).await;
            // The reject is observable via `b2bua_overload_rejected_total` (the
            // b2bua has no log framework wired; the sibling orphan-reject /
            // message-cap paths likewise signal only through metrics). The
            // `reason`/`retry_after_sec` are carried on the 503 itself (Reason +
            // Retry-After) for the caller and any wire trace.
            ctx.metrics.bump_overload_rejected();
            // ORPHAN TEARDOWN (leak fix — same class the None-branch below fixes).
            // This shed is *stateless at the call layer*: no `build_initial_call`/
            // `create`, so no dialog/CDR/limiter/replicated state is born. BUT the
            // event was dispatched into a fresh per-call queue (one `bump_creation`
            // — the `peek().is_some()` retransmit guard above guarantees this is a
            // brand-new call_ref, so `dispatch` allocated the queue + worker) and
            // `process` took the per-call lock above. Nothing will ever emit
            // `RemoveCall`, so the `locks`-map entry, the unmatched creation, and
            // the idle worker would all strand — under sustained overload this
            // ratchets `store_locks`/`b2bua_active_calls` exactly like the orphan
            // storm. Reclaim through the one teardown executor (`ReleaseKind::Orphan`
            // — `discard_orphan` only frees the lock entry, NO store mutation, so no
            // spurious reverse-propagated delete for a call we never held) so the
            // worker exits and `removals` balances `creations`. Drop our guard first
            // (mirrors the None-branch precedent ~30 lines below).
            drop(_guard);
            release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
            return;
        }
        // Counter published on X-Overload (`adm`). Emergency admits are NOT counted
        // on `adm` — the LB's AIMD caps non-emergency traffic only (TS contract) —
        // but ARE tallied on their own `b2bua_emergency_admitted_total` counter so
        // the emergency-admit branch is observable (it would otherwise be uncounted).
        if is_emergency {
            ctx.overload.increment_emergency_admitted();
        } else {
            ctx.overload.increment_non_emergency_admitted();
        }

        let call = build_initial_call(&req, src, &ctx.config, now_ms);
        ctx.state.create(call.clone());
        // RFC 3261 §8.1.1.3: a dialog-forming INVITE MUST carry a From tag. The
        // caller's From tag IS the a-leg dialog's remote tag, so admitting a
        // tag-less INVITE would seed an un-probeable a-leg dialog (its in-dialog
        // keepalive OPTIONS could never be built — see `send_request_to_leg`),
        // producing the "OPTIONS to called, not calling" asymmetry that also
        // round-trips through HA hydration. Reject malformed at ingest instead.
        // Created-then-rejected mirrors the decision-reject path below so the
        // Terminated invariant reaps the call + propagates the delete.
        if call.a_leg.from_tag.is_empty() {
            let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
            let rejected = crate::initial_invite::reject_call(
                call.clone(),
                &a_invite,
                400,
                Some("Bad Request - missing From tag".into()),
                None,
                &[],
                &ctx.id_gen,
                now_ms,
            );
            crate::rules::invariants::enforce(&ctx.obligations, &call, crate::rules::invariants::finalize(rejected), now_ms, true)
        } else {
            let result =
                handle_initial_invite(call.clone(), ctx.decision.as_ref(), ctx.limiter.as_ref(), &ctx.config, &ctx.id_gen, &ctx.services, now_ms).await;
            crate::rules::invariants::enforce(&ctx.obligations, &call, crate::rules::invariants::finalize(result), now_ms, true)
        }
    } else {
        // In-dialog: peek the in-memory map, falling back to the acting-backup
        // takeover read-path (S10b) — hydrate the call from the replica store's
        // backup partition when the primary crashed and the proxy failed this
        // dialog over to us. A primary-role miss then tries the ON-DEMAND
        // reclaim below; only a genuine orphan (no replica anywhere) rejects.
        let mut call = match ctx.state.hydrate_from_replica(&call_ref).await {
            Some((c, fresh, skew_offset_ms)) => {
                // Failover timer re-arm: per-call timers (keepalive, global
                // duration, …) live in this node's in-memory `TimerService`, NOT
                // in the replicated call state — so a call freshly materialized
                // from a backup arrives with no live timers on THIS node. Re-arm
                // its serialized timer intents (`call.timers`, which IS
                // replicated) into the local driver, exactly once, on the
                // hydration that created it. Without this the hydrated call has
                // no keepalive (a dead peer is never probed) and no duration cap
                // (never reaped) → `b2bua_active_calls` leaks on the takeover
                // node — the failover analogue of the steady-state no-BYE leak.
                // `restore` past-due entries fire immediately (the keepalive then
                // re-arms itself on the next interval via the `keepalive` rule);
                // re-arming is idempotent — any subsequent rule-emitted
                // `ScheduleTimer` for the same id supersedes it via the driver's
                // epoch bump. Skipped for `fresh == false` (the call was already
                // resident and its timers are already live) to avoid double-arm.
                if fresh {
                    // Mark this as a live acting-backup takeover copy (ADR-0014)
                    // and ARM the self-release notice: the txn layer will send a
                    // `CallQuiesced` once the transaction(s) we serve for this call
                    // all reach a terminal state, at which point the router sheds
                    // the live copy (keeping the `bak:` replica).
                    ctx.state.mark_takeover(&call_ref);
                    // Restore-hygiene seam (clock-skew hardening): re-anchor the
                    // failed-over timers by the persisted receive-time skew offset,
                    // drop the stale in-flight `KeepaliveTimeout` (the OPTIONS it
                    // guarded died with the crashed primary), and apply the
                    // deep-past-due keepalive floor — so no immediate OPTIONS races
                    // the failed-over re-INVITE (endurance-20260630). No cohort here
                    // (single call), so no smoothing.
                    let mut takeover_timers = c.timers.clone();
                    sanitize_restored_timers(
                        &mut takeover_timers,
                        &call_ref,
                        ctx.clock.now_ms(),
                        Some(skew_offset_ms),
                        ctx.config.keepalive_interval_sec * 1000,
                        None,
                    );
                    ctx.timers.restore(takeover_timers, call_ref.clone()).await;
                    let _ = ctx.txn.watch_self_release(&call_ref).await;
                }
                c
            }
            // REBOOTED-PRIMARY on-demand reclaim (ADR-0014). An in-dialog
            // request (BYE / re-INVITE / UPDATE) can race the bulk `ReclaimAll`
            // sweep on a rebooted primary: the body sits fully reclaimable in
            // `pri:{self}` (the bootstrap imported it) but the serial sweep has
            // not materialised it yet — and the only other materialisation
            // trigger was a backup's reverse-flush `ReclaimCall` push, never an
            // arriving request. Refusing to look 481'd a healthy long-hold call
            // whose state lives RIGHT HERE (the endurance long-call-on-reboot /
            // re-INVITE-mid-reclaim loss). Materialise on demand, exactly as the
            // reactive straggler path does (timers restored, no smoothing — one
            // call), under the per-call guard we already hold; the bulk sweep's
            // own `materialize_if_absent` keeps the two passes idempotent. NOT a
            // takeover: this is our own call — no mark, no self-release watch.
            // A call never imported into `pri:{self}` (its only copy is the
            // peer's `bak:{self}`) still orphans — recovering THAT population
            // needs an on-demand pull from the peer (s11 CASE B, open).
            None => match ctx.state.peek_reclaimable(&call_ref).await {
                Some((call, skew_offset_ms)) => {
                    let mut timers = call.timers.clone();
                    // Same restore-hygiene seam as the bulk/reactive reclaim paths:
                    // re-anchor by the skew offset, drop the stale timeout, apply
                    // the deep-past-due floor. No cohort (one call) → no smoothing.
                    sanitize_restored_timers(
                        &mut timers,
                        &call_ref,
                        ctx.clock.now_ms(),
                        Some(skew_offset_ms),
                        ctx.config.keepalive_interval_sec * 1000,
                        None,
                    );
                    if ctx.state.materialize_if_absent(call.clone()) {
                        ctx.timers.restore(timers, call_ref.clone()).await;
                        ctx.metrics.bump_repl_reclaimed();
                    }
                    call
                }
                None => {
                    maybe_reject_orphan(ctx, &event).await;
                    // ORPHAN TEARDOWN (leak fix). This event was dispatched into a
                    // per-call queue — one `bump_creation` (→ `b2bua_active_calls`)
                    // — and `process` took the per-call lock above, but it resolved
                    // to NO live call. Nothing will ever emit `RemoveCall`, and a
                    // per-call dispatch worker exits ONLY on poison (its sender
                    // lives in the queue map, so the channel never closes on its
                    // own). So the queue, its idle task, the unmatched creation,
                    // and the lock entry would ALL leak permanently — ~1 per
                    // orphan, which a mass-orphan failover (thousands of in-dialog
                    // BYEs hitting a rebooted worker whose calls were never
                    // reclaimed) turns into a multi-thousand `active_calls` +
                    // `store_locks` ratchet that never drains. Release through the
                    // one teardown executor (`ReleaseKind::Orphan` — no store
                    // mutation, so no spurious reverse-propagated delete) so the
                    // worker exits and `removals` balances `creations`. Drop our
                    // guard first so the poisoned worker never contends on this
                    // call_ref's (now removed) lock.
                    drop(_guard);
                    release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
                    return;
                }
            },
        };
        // The limiter-refresh timer is async (an HTTP call to migrate holds), so
        // it is handled outside the synchronous rule chain — like initial-INVITE.
        if matches!(
            &event,
            CallEvent::Timer {
                timer_type: TimerType::LimiterRefresh,
                ..
            }
        ) {
            let before = call.clone();
            let res = handle_limiter_refresh(ctx, call, now_ms).await;
            crate::rules::invariants::enforce(&ctx.obligations, &before, crate::rules::invariants::finalize(res), now_ms, true)
        } else {
            // ── MAX_MESSAGES_PER_CALL cap-defense ──────────────────────────
            // Port of the TS `SipRouter` per-event `messageCount` bump + cap
            // check (src/sip/SipRouter.ts). EVERY in-dialog rule-chain event
            // bumps the counter; if the bump crosses `max_messages_per_call`
            // and the handler did not itself terminate the call, append a
            // begin-termination so a runaway dialog (re-INVITE/OPTIONS storm,
            // glare loop, a peer that never stops) is torn down instead of
            // processing unbounded in-dialog events forever — each of which
            // allocates a txn (`set_txn`), a working `Call` clone, and a store
            // body. Initial-INVITE (the `if` arm) and the async limiter-refresh
            // do NOT count, mirroring TS (only `handlers.inDialog` events).
            // The bump rides the existing per-event flush — `message_count` adds
            // no extra replication traffic (it mutates with the CSeq/state the
            // event already changes). Order matches TS: bump + capture
            // `cap_exceeded` BEFORE the handler runs, terminate AFTER, so the
            // in-flight event (e.g. relaying this re-INVITE's response) is still
            // serviced before teardown.
            let bumped = call.message_count.unwrap_or(0) + 1;
            call.message_count = Some(bumped);
            let cap_exceeded = bumped > ctx.config.max_messages_per_call as i64
                && !matches!(
                    call.state,
                    CallModelState::Terminating | CallModelState::Terminated
                );
            let rule_ctx = RuleContext {
                call: RuleCall::new(&call),
                call_ref: &call_ref,
                event: &event,
                source_leg_id: &res.source_leg_id,
                direction: res.direction,
                now_ms,
                config: &ctx.config,
            };
            let exec = ActionExecutor {
                config: &ctx.config,
                id_gen: &ctx.id_gen,
                now_ms,
            };
            let mut result = execute_rules(&ctx.rules, &call, &rule_ctx, &exec, &ctx.obligations);
            if cap_exceeded
                && !matches!(
                    result.call.state,
                    CallModelState::Terminating | CallModelState::Terminated
                )
            {
                // Tear the runaway call down through the standard executor so
                // per-leg BYE/CANCEL, dialog-tag ownership and the safety-timer
                // contract apply exactly as a rule-driven termination would. The
                // RFC-3326 cause rides the reason (must start with "SIP").
                let cap_ctx = RuleContext {
                    call: RuleCall::new(&result.call),
                    call_ref: &call_ref,
                    event: &event,
                    source_leg_id: &res.source_leg_id,
                    direction: res.direction,
                    now_ms,
                    config: &ctx.config,
                };
                // An UNANSWERED a-leg (still trying/early) has no final response
                // yet: `begin_termination` assumes the firing *rule* already
                // replied (as `setup-timeout` does via RespondToALeg) and so only
                // settles the leg's disposition. The cap fires from the router,
                // not a rule, so nobody replied — without this the caller's INVITE
                // hangs until its own Timer B and the limiter slot is held until
                // the ~32 s TerminatingTimeout. Send the 503 cap cause as the
                // caller's final so the INVITE resolves now and the call
                // terminates (decrementing the limiter) immediately. An answered
                // a-leg (confirmed) takes the BYE path inside begin_termination —
                // no response then. NB: the TS port (`SipRouter.ts`) runs
                // begin-termination alone and carries the same latent gap — the
                // fix should be ported back.
                let mut cap_actions = Vec::new();
                if matches!(result.call.a_leg.state, LegState::Trying | LegState::Early) {
                    cap_actions.push(RuleAction::RespondToALeg {
                        status: 503,
                        reason: "Service Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    });
                }
                cap_actions.push(RuleAction::BeginTermination {
                    reason: Some("SIP;cause=503;text=\"message-cap-exceeded\"".into()),
                });
                let cap = exec.execute(&cap_actions, &result.call, &cap_ctx);
                result.call = cap.call;
                result.effects.critical.extend(cap.effects.critical);
                result.effects.outbound.extend(cap.effects.outbound);
                result.effects.soft.extend(cap.effects.soft);
                result.effects.buffered.extend(cap.effects.buffered);
                result.effects.fire_and_forget.extend(cap.effects.fire_and_forget);
                ctx.metrics.bump_message_cap_terminated();
            }
            result
        }
    };

    // Per-peer keepalive-timeout attribution (observability only;
    // b2bua_peer_failures_total{...,kind="keepalive_timeout"}). The genuine
    // no-200 keepalive timeout is the `KeepaliveTimeout` timer firing for a
    // specific leg L (`leg_id`): the `keepalive-timeout` rule tears L down (no
    // BYE to L — it is unresponsive) and BYEs the SURVIVING leg. So we MUST NOT
    // attribute to the outbound BYE's destination (that is the surviving, often
    // healthy, leg's hop and mis-classifies internal/external). Attribute to the
    // FAILED leg L's OWN egress-aware next hop — the exact hop the unanswered
    // OPTIONS went to. If L or its dialog can't be resolved we record nothing (no
    // fabricated address). Distinct from the reclaim-time stale drop
    // (`drop_stale_keepalive_timeout`), which never reaches this event path.
    if let CallEvent::Timer { timer_type: TimerType::KeepaliveTimeout, leg_id, .. } = &event {
        if let Some((host, port)) = keepalive_timeout_peer(&ctx.config, &result.call, leg_id.as_deref()) {
            use std::net::ToSocketAddrs;
            if let Ok(mut addrs) = (host.as_str(), port).to_socket_addrs() {
                if let Some(dest) = addrs.next() {
                    ctx.metrics.record_peer_failure(
                        &dest,
                        classify_b2bua_peer(&ctx.config, &dest),
                        crate::peer_failures::PeerFailureKind::KeepaliveTimeout,
                    );
                }
            }
        }
    }

    process_result(ctx, &call_ref, result, now_ms).await;
}

/// Handle a `LimiterRefresh` timer: migrate every live hold to the current
/// window (an async `/v1/refresh` call), update the stored windows, and re-arm
/// the timer while the call is alive. Port of `FrameworkLimiterRefresh.ts`.
async fn handle_limiter_refresh(ctx: &Arc<RouterCtx>, mut call: Call, now_ms: i64) -> HandlerResult {
    use crate::limiter::LimiterHold;

    let holds: Vec<LimiterHold> = call
        .limiter_entries
        .iter()
        .filter(|e| e.increment_succeeded != Some(false))
        .map(|e| LimiterHold {
            limiter_id: e.limiter_id.clone(),
            window: e.origin_window,
        })
        .collect();

    let mut fx = HandlerEffects::new();
    if holds.is_empty() {
        return HandlerResult { call, effects: fx };
    }

    // All holds migrate to the same current window; adopt it for every live
    // entry. On a backend failure `refresh` returns the holds unchanged, so the
    // windows simply stay put and we retry next cycle.
    let updated = ctx.limiter.refresh(&holds).await;
    if let Some(new_window) = updated.first().map(|h| h.window) {
        for e in call.limiter_entries.iter_mut() {
            if e.increment_succeeded != Some(false) {
                e.origin_window = new_window;
            }
        }
    }

    if call.state == CallModelState::Active {
        let entry = TimerEntry {
            id: format!("{:?}", TimerType::LimiterRefresh),
            timer_type: TimerType::LimiterRefresh,
            fire_at: now_ms + ctx.config.limiter_refresh_sec * 1000,
            leg_id: None,
        };
        call.timers =
            call::helpers::replace_timer_by_id(std::mem::take(&mut call.timers), entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }

    HandlerResult { call, effects: fx }
}

/// A request for a vanished call → 481 (ACK/responses are silently dropped).
async fn maybe_reject_orphan(ctx: &RouterCtx, event: &CallEvent) {
    if let CallEvent::Sip { message, src } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method != "ACK" {
                let resp = generate_response(
                    req,
                    481,
                    "Call/Transaction Does Not Exist",
                    &GenerateResponseOpts::default(),
                );
                let _ = ctx.txn.send_response(resp, *src).await;
            }
        }
    }
}

/// Interpret a handler result: persist → critical → outbound → soft → buffered.
async fn process_result(ctx: &Arc<RouterCtx>, call_ref: &str, result: HandlerResult, now_ms: i64) {
    // Persist first (the source's invariant: state lands before effects run).
    ctx.state.update(result.call.clone());

    // Model Y (ADR-0020 X3 amended): an acting-backup **takeover copy** that
    // reaches Terminated DEFERS the discharge to the live primary. It reverse-
    // flushes the terminal body — so the primary's Reclaim-tail reconcile
    // (`reconcile_reverse_flush`) folds it in and discharges it **exactly once** —
    // then self-releases its live copy. It writes **NO** CDR, releases **NO**
    // limiter hold, propagates **NO** delete here (that is the primary's sole
    // authority). If the primary never reconciles (crashed for good, never returning
    // inside the replica TTL), the retained `bak:` replica is silently evicted by the
    // periodic reap and the CDR/limiter cleanup is LOST — the accepted double-failure
    // (primary down AND never returns). This replaces a2dcf4c's
    // discharge-on-CallQuiesced for a takeover copy: the backup is never an
    // independent CDR/limiter writer, so exactly-once holds by construction (no
    // cross-node idempotency). A primary-served (non-takeover) terminal falls through
    // to the normal discharge below.
    if result.call.state == CallModelState::Terminated && ctx.state.is_takeover(call_ref) {
        // Reverse-flush the Terminated body held with the normal replica TTL
        // (`reboot_budget`): a live primary reconciles + forward-deletes it within ~1
        // poll; a rebooting primary still has its full reclaim window to fold and
        // discharge it. The primary is the sole discharge authority either way.
        ctx.state.flush(&result.call);
        release_call(ctx, call_ref, ReleaseKind::SelfRelease).await;
        return;
    }

    // Replicate a non-terminated, backed-up call to its peer after each
    // authoritative mutation (the S10 flush-on-mutation wiring point —
    // `replication.rs` defers sourcing the backup peer to S10, which the
    // cookie-stamped `topology.bak` now provides). `CallState::flush` is a no-op
    // for calls with no replicable topology, so the non-HA path is unchanged; for
    // a backed-up call it routes through the S8 write-side policy (Forward when
    // primary, Reverse when acting-backup) so the backup holds the latest state.
    // The flush rides the buffered terminate-writer (non-blocking).
    //
    // `Terminating` MUST flush too, not just `Active`: a teardown-in-progress
    // carries authoritative state the replica needs — the b-leg `ByeSent`
    // disposition and its bumped `local_cseq`. Skipping it (the old `== Active`
    // gate) stranded that progress in the live copy only, so an acting-backup
    // whose primary had crashed never propagated it; a reclaim racing the
    // in-flight BYE then pulled the STALE `Active` snapshot, restarted
    // termination, and re-sent the BYE at the *reused* CSeq a real UAS drops
    // (matrix cells C7/RFC). Only `Terminated` is excluded — it takes the
    // `RemoveCall` delete path below instead.
    if matches!(
        result.call.state,
        CallModelState::Active | CallModelState::Terminating
    ) && result
        .call
        .topology
        .as_ref()
        .is_some_and(|t| !t.bak.is_empty())
    {
        ctx.state.flush(&result.call);
    }

    // The terminal `RemoveCall` is interpreted LAST — after the buffered
    // `WriteCdr` enqueue (ADR-0020 X2). It used to run here in the critical
    // lane, which propagated the replica delete *before* the CDR was even
    // enqueued: a failure in that window erased the call everywhere (including
    // the backup Element) with no CDR. Deferring only delays the eviction /
    // txn-cancel by the in-process lanes below; the call is already
    // unreachable for new work (its state is persisted and terminal).
    let mut remove_call = false;
    for eff in &result.effects.critical {
        match eff {
            CriticalStateEffect::ScheduleTimer(entry) => {
                ctx.timers.schedule(entry.clone(), call_ref.to_string()).await;
            }
            CriticalStateEffect::CancelTimer { id } => {
                ctx.timers.cancel(call_ref.to_string(), id.clone()).await
            }
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::Flush => ctx.state.flush(&result.call),
            CriticalStateEffect::RemoveCall => remove_call = true,
        }
    }

    // Sent SIP is liveness too (ADR-0020 X4 refinement): a turn that puts a
    // message on the wire (a keepalive OPTIONS, a relayed response, a teardown
    // BYE/CANCEL) stamps the ledger alongside received traffic, so the reaper
    // never preempts a teardown that is legitimately waiting on a slow peer.
    // Wire-silent turns (LimiterRefresh, absorbed events) stamp nothing. Only
    // for a still-live call — a terminated result is being released below.
    if !result.effects.outbound.is_empty() && result.call.state != CallModelState::Terminated {
        ctx.state.touch(call_ref, now_ms);
    }

    for eff in &result.effects.outbound {
        let dest: SocketAddr = match format!("{}:{}", eff.destination.0, eff.destination.1).parse() {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Meter outbound requests we originate/relay (the in-dialog keepalive
        // OPTIONS lands here) — pairs with inbound responses_total{OPTIONS,200} to
        // isolate the keepalive round-trip (sent vs answered) on the b2bua itself.
        if let OutboundBody::Request(req) = &eff.body {
            ctx.metrics.record_request_out(req.method.as_str());
        }
        match (&eff.body, &eff.mode) {
            // A 2xx retransmit (RFC 3261 §13.3.1.4) must bypass the server txn:
            // the a-leg INVITE server txn is already `Completed`, so the txn layer
            // would DROP a second final on `send_response`. Send it raw.
            (OutboundBody::Response(resp), OutboundTxnMode::Raw) => {
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Response(resp.clone())), dest).await;
            }
            (OutboundBody::Response(resp), _) => { let _ = ctx.txn.send_response(resp.clone(), dest).await; }
            (OutboundBody::Request(req), OutboundTxnMode::NewClient(kind)) => {
                let _ = ctx.txn.send_request(req.clone(), dest, *kind).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::Raw) => {
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::ServerResponse) => {
                // A request tagged ServerResponse is a misuse; send raw as a fallback.
                let _ = ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await;
            }
        }
    }

    for eff in &result.effects.soft {
        match eff {
            SoftBoundedEffect::DecrementLimiter { limiter_id, window } => {
                ctx.limiter
                    .release(&[crate::limiter::LimiterHold {
                        limiter_id: limiter_id.clone(),
                        window: *window,
                    }])
                    .await
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&result.call, now_ms).await,
        }
    }

    // Terminal eviction last of all (ADR-0020 X2): the CDR is enqueued before
    // the call — and its replicated Element — ceases to exist anywhere.
    if remove_call {
        release_call(ctx, call_ref, ReleaseKind::Terminated).await;
    }

    // Fire-and-forget: detached async work that folds its result back into the
    // call via a re-entrant internal event (the REFER `/call/refer` round-trip,
    // and the generic re-enter path).
    for eff in result.effects.fire_and_forget {
        match eff {
            FireAndForgetEffect::ReferAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                // Call-scoped context is attached HERE (the framework holds the
                // authoritative call at dispatch); the seed rule's JSON carries
                // only the event-scoped facts.
                let snapshot = crate::decision::CallSnapshot::of(&result.call);
                tokio::spawn(async move {
                    // Deserialize the request the seed rule built (mirrors the
                    // TS POST body); call the decision backend; map to a
                    // `refer-http-result` internal event; re-enter the chain.
                    let mut req = parse_call_refer_request(&request);
                    req.snapshot = snapshot;
                    let (outcome, payload) = match ctx2.decision.call_refer(req).await {
                        Ok(CallReferResponse::Allow {
                            destination,
                            new_refer_to,
                            update_headers,
                            no_answer_timeout_sec,
                            callback_context,
                        }) => {
                            let mut p = serde_json::Map::new();
                            p.insert("action".into(), serde_json::json!("allow"));
                            p.insert(
                                "destination".into(),
                                serde_json::json!({
                                    "host": destination.host,
                                    "port": destination.port,
                                    "transport": destination.transport,
                                }),
                            );
                            if let Some(v) = new_refer_to {
                                p.insert("new_refer_to".into(), serde_json::json!(v));
                            }
                            if let Some(v) = update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            if let Some(v) = no_answer_timeout_sec {
                                p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
                            }
                            if let Some(v) = callback_context {
                                p.insert("callback_context".into(), serde_json::json!(v));
                            }
                            ("allow", serde_json::Value::Object(p))
                        }
                        Ok(CallReferResponse::Reject { code, reason }) => (
                            "reject",
                            serde_json::json!({ "reject_code": code, "reject_reason": reason }),
                        ),
                        Err(_) => ("error", serde_json::json!({})),
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "refer-http-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                    };
                    // Re-enter via the router's event channel rather than
                    // calling `on_event` directly: the `on_event → process →
                    // process_result → on_event` cycle has an opaque future type
                    // the compiler cannot prove `Send`. Routing the event back
                    // through `run`'s loop keeps re-entry single-threaded and
                    // breaks the recursion.
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                // Call-scoped context is attached HERE (the framework holds the
                // authoritative call at dispatch); the seed rule's JSON carries
                // only the event-scoped facts (origin, failed leg, sip headers).
                let snapshot = crate::decision::CallSnapshot::of(&result.call);
                tokio::spawn(async move {
                    // The seed rule's request JSON carries the failure context
                    // plus `failed_leg_id` (echoed back so the resolution rule
                    // can cancel the right no-answer timer / relay the failure).
                    let mut req = parse_call_failure_request(&request);
                    req.snapshot = snapshot.clone();
                    let failed_leg_id = request
                        .get("failed_leg_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    use crate::decision::CallTreatment;
                    // Failover-route/initial-route parity: a failover Route is
                    // admitted against the call limiter here (the rule layer is
                    // sync), and a limiter reject re-consults `/call/failure`
                    // with origin `call_limiter` — the same bounded chain
                    // `apply_route` runs for the initial route. A hold admitted
                    // here is folded into the call by the resolution rule
                    // (`RecordLimiterHolds`); if the call dies before the fold
                    // lands, the orphaned INCR ages out of the limiter window.
                    let mut depth: u32 = 0;
                    let (outcome, payload) = loop {
                        break match ctx2.decision.call_failure(req).await {
                        Ok(CallTreatment::Route(route)) => {
                            let mut admitted: Option<(Vec<(String, i64)>, i64)> = None;
                            if !route.call_limiter.is_empty() {
                                let entries: Vec<crate::limiter::LimiterEntry> = route
                                    .call_limiter
                                    .iter()
                                    .map(|e| crate::limiter::LimiterEntry {
                                        id: e.id.clone(),
                                        limit: e.limit,
                                    })
                                    .collect();
                                match ctx2.limiter.admit(&entries).await {
                                    crate::limiter::AdmitOutcome::Admitted { window } => {
                                        admitted = Some((
                                            route
                                                .call_limiter
                                                .iter()
                                                .map(|e| (e.id.clone(), e.limit))
                                                .collect(),
                                            window,
                                        ));
                                    }
                                    // Fail open: no holds recorded (parity with
                                    // the initial path's fail-open policy).
                                    crate::limiter::AdmitOutcome::Unavailable => {}
                                    crate::limiter::AdmitOutcome::Rejected { limiter_id } => {
                                        if route.callback_context.is_some()
                                            && depth
                                                < crate::decision::apply_route::MAX_LIMITER_FAILOVER
                                        {
                                            depth += 1;
                                            req = crate::decision::CallFailureRequest {
                                                callback_context: route.callback_context.clone(),
                                                failure: crate::decision::FailureInfo {
                                                    origin: "call_limiter".to_string(),
                                                    limiter_id: Some(limiter_id),
                                                    failed_leg_id: (!failed_leg_id.is_empty())
                                                        .then(|| failed_leg_id.clone()),
                                                    ..Default::default()
                                                },
                                                snapshot: snapshot.clone(),
                                            };
                                            continue;
                                        }
                                        // Chain exhausted / no context → the
                                        // initial path's terminal limiter
                                        // treatment (486 Busy Here).
                                        break (
                                            "reject",
                                            serde_json::json!({
                                                "code": 486,
                                                "reason": "Busy Here",
                                                "failed_leg_id": failed_leg_id,
                                            }),
                                        );
                                    }
                                }
                            }
                            let mut p = serde_json::Map::new();
                            p.insert(
                                "destination".into(),
                                serde_json::json!({
                                    "host": route.destination.host,
                                    "port": route.destination.port,
                                }),
                            );
                            if let Some(v) = route.new_ruri {
                                p.insert("new_ruri".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.new_from {
                                p.insert("new_from".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.new_to {
                                p.insert("new_to".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route
                                .no_answer_timeout_sec
                                .or(route.features.no_answer_timeout_sec)
                            {
                                p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.callback_context {
                                p.insert("callback_context".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            // Route parity: the fields the initial `apply_route`
                            // honors, forwarded to the resolution rule.
                            if let Ok(v) = serde_json::to_value(&route.features) {
                                p.insert("features".into(), v);
                            }
                            if !route.service_ext.is_empty() {
                                p.insert(
                                    "service_ext".into(),
                                    serde_json::Value::Object(
                                        route.service_ext.into_iter().collect(),
                                    ),
                                );
                            }
                            match route.update_body {
                                crate::decision::BodyUpdate::Keep => {}
                                crate::decision::BodyUpdate::Drop => {
                                    p.insert("update_body".into(), serde_json::Value::Null);
                                }
                                crate::decision::BodyUpdate::Replace(s) => {
                                    p.insert("update_body".into(), serde_json::json!(s));
                                }
                            }
                            if let Some((entries, window)) = admitted {
                                let entries: Vec<serde_json::Value> = entries
                                    .iter()
                                    .map(|(id, limit)| serde_json::json!({"id": id, "limit": limit}))
                                    .collect();
                                p.insert(
                                    "call_limiter".into(),
                                    serde_json::json!({"window": window, "entries": entries}),
                                );
                            }
                            ("failover", serde_json::Value::Object(p))
                        }
                        // Decision-authored reject — the plan declined to fail over
                        // and supplied its own final failure (code/reason/headers).
                        Ok(CallTreatment::Reject(rj)) => {
                            let mut p = serde_json::Map::new();
                            p.insert("code".into(), serde_json::json!(rj.reject_code));
                            if let Some(v) = rj.reject_reason {
                                p.insert("reason".into(), serde_json::json!(v));
                            }
                            if let Some(v) = rj.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("reject", serde_json::Value::Object(p))
                        }
                        // Decision-authored 3xx redirect with a Contact list.
                        Ok(CallTreatment::Redirect(rd)) => {
                            let mut p = serde_json::Map::new();
                            p.insert("code".into(), serde_json::json!(rd.code));
                            if let Some(v) = rd.reason {
                                p.insert("reason".into(), serde_json::json!(v));
                            }
                            let contacts: Vec<serde_json::Value> = rd
                                .contacts
                                .iter()
                                .map(|c| serde_json::json!({ "uri": c.uri, "q": c.q }))
                                .collect();
                            p.insert("contacts".into(), serde_json::json!(contacts));
                            if let Some(v) = rd.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("redirect", serde_json::Value::Object(p))
                        }
                        // Explicit `Relay`, or a backend error → relay the original
                        // b-leg failure (response path) + tear the call down. Echo
                        // the failure's status/reason the seed stashed for the relay.
                        Ok(CallTreatment::Relay) | Err(_) => {
                            let mut p = serde_json::Map::new();
                            if let Some(v) = request.get("sip_code") {
                                p.insert("status".into(), v.clone());
                            }
                            if let Some(v) = request.get("sip_reason") {
                                p.insert("reason".into(), v.clone());
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("terminate", serde_json::Value::Object(p))
                        }
                        };
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "call-failure-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                    };
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::Reenter(ev) => {
                let _ = ctx.reentry_tx.send(*ev);
            }
        }
    }
}

/// Rebuild a [`CallReferRequest`] from the JSON the seed rule emitted.
fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let sip_headers = v
        .get("sip_headers")
        .and_then(|x| x.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers,
        snapshot: crate::decision::CallSnapshot::default(),
    }
}

/// Rebuild a [`CallFailureRequest`] from the JSON the seed rule emitted. The
/// call-scoped `snapshot` is not part of the rule JSON — the dispatch site
/// attaches it from the authoritative call.
fn parse_call_failure_request(v: &serde_json::Value) -> crate::decision::CallFailureRequest {
    crate::decision::CallFailureRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        failure: crate::decision::FailureInfo {
            origin: v
                .get("origin")
                .and_then(|x| x.as_str())
                .unwrap_or("external")
                .to_string(),
            status_code: v
                .get("sip_code")
                .and_then(|x| x.as_u64())
                .map(|c| c as u16),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
            failed_leg_id: v
                .get("failed_leg_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // `[[name, value], …]` — wire order and duplicates preserved.
            sip_headers: v
                .get("sip_headers")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|pair| {
                            let p = pair.as_array()?;
                            Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        snapshot: crate::decision::CallSnapshot::default(),
    }
}

/// Extract `(cr, lg)` from a Via header value's `;cr=`/`;lg=` params.
fn via_cr_lg(via: Option<&str>) -> Option<(Option<String>, String)> {
    let via = via?;
    if !via.contains("cr=") && !via.contains("lg=") {
        return None;
    }
    let mut cr = None;
    let mut lg = "a".to_string();
    for part in via.split(';').skip(1) {
        let (k, v) = part.split_once('=').unwrap_or((part.trim(), ""));
        match k.trim() {
            "cr" => cr = Some(crate::stack_identity::decode_param(v.trim())),
            "lg" => lg = crate::stack_identity::decode_param(v.trim()),
            _ => {}
        }
    }
    Some((cr, lg))
}

#[cfg(test)]
mod reclaim_timer_tests {
    use super::*;
    use crate::timers::TimerService;
    use std::time::Duration;

    #[test]
    fn classify_b2bua_peer_internal_iff_outbound_proxy() {
        use crate::peer_failures::PeerScope;
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));
        let proxy: SocketAddr = "10.0.0.9:5060".parse().unwrap();
        let other: SocketAddr = "203.0.113.7:5060".parse().unwrap();
        assert_eq!(classify_b2bua_peer(&config, &proxy), PeerScope::Internal);
        assert_eq!(classify_b2bua_peer(&config, &other), PeerScope::External);
        // With no outbound proxy configured, every peer is external.
        config.b2b_outbound_proxy = None;
        assert_eq!(classify_b2bua_peer(&config, &proxy), PeerScope::External);
    }

    // A hostname-configured outbound proxy resolving to the candidate's IP+port
    // must classify Internal (BUG #2: the old `parse::<SocketAddr>()` on a
    // hostname always failed → everything fell to External, so internal pinning
    // never fired off-cluster). `localhost` is a stable, network-free resolution.
    #[test]
    fn classify_b2bua_peer_resolves_a_hostname_outbound_proxy() {
        use crate::peer_failures::PeerScope;
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("localhost".to_string(), 5060));
        // localhost resolves to 127.0.0.1 (and/or ::1); the loopback v4 candidate
        // at the configured port must be classified Internal.
        let loopback: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let other: SocketAddr = "127.0.0.1:5999".parse().unwrap();
        assert_eq!(
            classify_b2bua_peer(&config, &loopback),
            PeerScope::Internal,
            "a hostname-configured outbound proxy must resolve and pin internal",
        );
        assert_eq!(
            classify_b2bua_peer(&config, &other),
            PeerScope::External,
            "a different port is not the proxy",
        );
    }

    // BUG #1: a B-leg keepalive timeout must attribute to the FAILED b-leg's
    // egress hop (its OPTIONS went through the outbound proxy → internal), NOT to
    // the surviving a-leg's hop (the BYE destination this turn produced). Drives
    // `keepalive_timeout_peer` directly — the pure helper the router uses.
    #[test]
    fn keepalive_timeout_attributes_to_the_failed_b_leg_egress_hop() {
        let mut config = B2buaConfig::default();
        config.b2b_outbound_proxy = Some(("10.0.0.9".to_string(), 5060));

        // a-leg dialog: remote target is the external UAC (the surviving leg's hop).
        let a_dialog = test_dialog("203.0.113.7", 5060, &[]);
        // b-leg dialog: empty route set + b-leg → egress bootstraps through the
        // outbound proxy (the wire dest the unanswered OPTIONS used).
        let b_dialog = test_dialog("10.244.2.7", 5060, &[]);

        let call = test_call(vec![a_dialog], vec![("b-1", vec![b_dialog])]);

        // The KeepaliveTimeout fired for the b-leg.
        let hop = keepalive_timeout_peer(&config, &call, Some("b-1"))
            .expect("b-leg dialog resolves");
        assert_eq!(
            hop,
            ("10.0.0.9".to_string(), 5060),
            "BUG #1: attribute to the FAILED b-leg's egress hop (the outbound proxy), \
             not the surviving a-leg's external hop",
        );
        assert_eq!(
            classify_b2bua_peer(&config, &"10.0.0.9:5060".parse().unwrap()),
            crate::peer_failures::PeerScope::Internal,
            "the failed b-leg's egress hop classifies internal",
        );

        // Sanity: the a-leg's own hop is the external UAC (what the OLD buggy code
        // would have mis-attributed the b-leg failure to).
        assert_eq!(
            keepalive_timeout_peer(&config, &call, Some("a")).unwrap(),
            ("203.0.113.7".to_string(), 5060),
        );

        // Unresolvable leg / no leg_id → record nothing (no fabricated address).
        assert!(keepalive_timeout_peer(&config, &call, Some("b-99")).is_none());
        assert!(keepalive_timeout_peer(&config, &call, None).is_none());
    }

    fn test_dialog(remote_host: &str, remote_port: u16, route_set: &[&str]) -> call::Dialog {
        call::Dialog {
            sip: call::StackDialog {
                call_id: "cid@x".into(),
                local_tag: "ltag".into(),
                remote_tag: "rtag".into(),
                local_uri: "sip:svc@b2bua".into(),
                remote_uri: format!("sip:peer@{remote_host}"),
                remote_target: format!("sip:peer@{remote_host}:{remote_port}"),
                local_cseq: 1,
                route_set: route_set.iter().map(|s| s.to_string()).collect(),
            },
            ext: call::B2buaDialogExt {
                remote_cseq: None,
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
            },
        }
    }

    fn test_leg(leg_id: &str, dialogs: Vec<call::Dialog>) -> call::Leg {
        call::Leg {
            leg_id: leg_id.into(),
            call_id: "cid@x".into(),
            from_tag: "ftag".into(),
            source: call::RemoteInfo { address: "0.0.0.0".into(), port: 0 },
            state: LegState::Confirmed,
            disposition: call::LegDisposition::Bridged,
            dialogs,
            no_answer_timeout_sec: None,
            bye_disposition: None,
            local_uri: None,
            remote_uri: None,
            invite_request_uri: None,
            pending_invite_txn: None,
            ext: None,
            kind: None,
            adopted: None,
        }
    }

    fn test_call(a_dialogs: Vec<call::Dialog>, b_legs: Vec<(&str, Vec<call::Dialog>)>) -> Call {
        let mut call = build_initial_call(
            &crate::rules::relay::rebuild_a_leg_invite(&minimal_invite_snapshot()),
            "203.0.113.7:5060".parse().unwrap(),
            &B2buaConfig::default(),
            0,
        );
        call.a_leg.dialogs = a_dialogs;
        call.b_legs = b_legs.into_iter().map(|(id, ds)| test_leg(id, ds)).collect();
        call
    }

    fn minimal_invite_snapshot() -> call::ALegInviteSnapshot {
        call::ALegInviteSnapshot {
            uri: "sip:bob@10.244.2.7:5060".into(),
            headers: vec![
                call::SipHeader { name: "Via".into(), value: "SIP/2.0/UDP 203.0.113.7:5060;branch=z9hG4bKa".into() },
                call::SipHeader { name: "From".into(), value: "<sip:alice@203.0.113.7:5060>;tag=alice".into() },
                call::SipHeader { name: "To".into(), value: "<sip:bob@10.244.2.7:5060>".into() },
                call::SipHeader { name: "Call-ID".into(), value: "cid@x".into() },
                call::SipHeader { name: "CSeq".into(), value: "1 INVITE".into() },
                call::SipHeader { name: "Content-Length".into(), value: "0".into() },
            ],
            body: vec![],
        }
    }

    fn keepalive(fire_at: i64) -> TimerEntry {
        TimerEntry { id: "Keepalive".into(), timer_type: TimerType::Keepalive, fire_at, leg_id: None }
    }
    fn keepalive_timeout(leg: &str, fire_at: i64) -> TimerEntry {
        TimerEntry {
            id: format!("KeepaliveTimeout:{leg}"),
            timer_type: TimerType::KeepaliveTimeout,
            fire_at,
            leg_id: Some(leg.into()),
        }
    }

    // The reclaim/takeover hygiene: a snapshot caught mid-keepalive-round-trip
    // carries an armed `KeepaliveTimeout`; restoring it verbatim onto the
    // reclaiming/taking-over node fires it (its guarded OPTIONS died with the
    // crashed node) and BYEs a healthy long hold. The fix strips it; the next
    // `Keepalive` re-probes fresh. Asserts both the stripping AND that the
    // remaining `Keepalive` survives.
    #[test]
    fn drop_stale_keepalive_timeout_strips_only_the_timeout() {
        let mut timers = vec![
            keepalive(300_000),
            keepalive_timeout("a", 35_000),
            keepalive_timeout("b-1", 35_000),
            TimerEntry { id: "GlobalDuration".into(), timer_type: TimerType::GlobalDuration, fire_at: 3_600_000, leg_id: None },
        ];
        drop_stale_keepalive_timeout(&mut timers);
        assert!(
            timers.iter().all(|t| !matches!(t.timer_type, TimerType::KeepaliveTimeout)),
            "every KeepaliveTimeout is stripped from the reclaimed snapshot",
        );
        assert!(
            timers.iter().any(|t| matches!(t.timer_type, TimerType::Keepalive)),
            "the Keepalive (re-probe) timer is kept — the call re-probes on its own schedule",
        );
        assert!(
            timers.iter().any(|t| matches!(t.timer_type, TimerType::GlobalDuration)),
            "unrelated timers (GlobalDuration) are kept",
        );
    }

    // REPRO of the residual endurance loss at the timer layer: a *past-due*
    // `KeepaliveTimeout` restored from a pre-crash snapshot fires IMMEDIATELY
    // (`restore` clamps `fire_at <= now` to a next-tick fire) — this is the
    // event that drives `keepalive-timeout` → BYE on a healthy reclaimed call.
    // With the fix the stripped set restores NOTHING that fires at now=reclaim.
    #[tokio::test(start_paused = true)]
    async fn past_due_keepalive_timeout_fires_on_restore_without_the_fix() {
        let clock = Clock::test_at(0);
        // now is well past the snapshot's KeepaliveTimeout deadline (the dead
        // node armed it +120 s before its clock, which is in our past).
        tokio::time::advance(Duration::from_millis(200_000)).await;

        // WITHOUT the fix: the verbatim snapshot includes the past-due timeout.
        let (timers, mut fire_rx) = TimerService::spawn(clock.clone());
        let snapshot = vec![keepalive(500_000), keepalive_timeout("b-1", 120_000)];
        timers.restore(snapshot.clone(), "w0|cid|tag".into()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        let fired = fire_rx.recv().await.unwrap();
        match fired {
            CallEvent::Timer { timer_type, .. } => assert_eq!(
                timer_type,
                TimerType::KeepaliveTimeout,
                "BUG: the stale past-due KeepaliveTimeout fires on reclaim → spurious BYE",
            ),
            _ => panic!("expected a timer event"),
        }

        // WITH the fix: the same snapshot, stripped, fires NOTHING at reclaim time
        // (the future Keepalive is the only survivor and is far off).
        let (timers2, mut fire_rx2) = TimerService::spawn(clock);
        let mut fixed = snapshot;
        drop_stale_keepalive_timeout(&mut fixed);
        timers2.restore(fixed, "w0|cid|tag2".into()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            fire_rx2.try_recv().is_err(),
            "FIX: no spurious keepalive-timeout fires on reclaim; the call survives",
        );
    }

    // REPRO of the 2026-06-12 endurance throughput collapse at the smoothing
    // layer: a clean reboot rehydrates the whole partition at one instant with
    // future-dated keepalive deadlines clustered in one interval. WITHOUT the
    // future-dated branch they all keep the SAME `fire_at` and fire as one burst
    // a cadence later (the ~550 OPTIONS/s spike that saturated the front proxy);
    // WITH it each call's keepalive is spread into `[now, fire_at]` by a per-call
    // hash, so a 1000-call cohort no longer shares a single deadline.
    #[test]
    fn future_dated_keepalive_cohort_is_de_correlated() {
        let now = 1_000_000;
        let deadline = now + 300_000; // whole cohort clustered at +300 s (one interval)
        let speedup = 10;
        let mut fire_ats = std::collections::HashSet::new();
        for i in 0..1000 {
            // Distinct call_ref per call, identical clustered keepalive deadline.
            let call_ref = format!("w1|call-{i}|tag-{i}");
            let mut timers = vec![keepalive(deadline)];
            smooth_keepalives(&mut timers, &call_ref, now, 0, speedup, None);
            let fa = timers[0].fire_at;
            assert!(
                (now..=deadline).contains(&fa),
                "spread keepalive stays in [now, original deadline] (never delayed past it): {fa}",
            );
            fire_ats.insert(fa);
        }
        // De-correlation: a synchronised cohort would collapse to ONE deadline;
        // the fix scatters them across the interval (allow a few hash collisions).
        assert!(
            fire_ats.len() > 900,
            "cohort de-correlated: {} distinct fire_at over 1000 calls (was 1 before the fix)",
            fire_ats.len(),
        );
        // Determinism: re-running the same ref yields the SAME slot (idempotent
        // reboot re-pass — a second reclaim scan must not re-scatter live timers).
        let mut a = vec![keepalive(deadline)];
        let mut b = vec![keepalive(deadline)];
        smooth_keepalives(&mut a, "w1|call-7|tag-7", now, 0, speedup, None);
        smooth_keepalives(&mut b, "w1|call-7|tag-7", now, 0, speedup, None);
        assert_eq!(a[0].fire_at, b[0].fire_at, "stable_jitter is deterministic per call_ref");
    }

    // The past-due (overdue) path is unchanged by the refactor: oldest-first,
    // bounded to speedup× cadence — most-overdue fires first (smallest offset).
    #[test]
    fn past_due_keepalives_keep_oldest_first_schedule() {
        let now = 1_000_000;
        let l_max = 200_000; // most-overdue gap across the batch
        let speedup = 10;
        // Most-overdue (fire_at = now - 200s, l = l_max): offset (l_max-l)/speedup = 0.
        let mut oldest = vec![keepalive(now - 200_000)];
        smooth_keepalives(&mut oldest, "w1|a|a", now, l_max, speedup, None);
        assert_eq!(oldest[0].fire_at, now, "most-overdue re-probes first (offset 0)");
        // Least-overdue of the batch (fire_at = now - 100s, l = 100s): offset
        // (l_max - l)/speedup = (200s - 100s)/10 = 10s later.
        let mut newer = vec![keepalive(now - 100_000)];
        smooth_keepalives(&mut newer, "w1|b|b", now, l_max, speedup, None);
        assert_eq!(newer[0].fire_at, now + 10_000, "less-overdue drains later, bounded by speedup");
    }

    // ── clock-skew hardening: the restore-hygiene seam ──────────────────────

    /// Test #3 — smooth_keepalives / l_max classification over SKEW-CORRECTED
    /// offsets. A reclaimer anchored +45 s AHEAD of the origin reads a keepalive
    /// that is genuinely FUTURE-dated (fire_at = origin_now + 300 s) as if it were
    /// past-due, because the raw `fire_at` is in the reclaimer's past-frame. WITHOUT
    /// re-anchoring, `smooth_keepalives` would classify it past-due and compress it
    /// into the catch-up band (the 2026-06-12 OPTIONS burst). The seam re-anchors
    /// first (+45 s), so it is correctly seen as future-dated and de-correlated into
    /// `[now, fire_at]` — never crushed to `now`.
    #[test]
    fn seam_reanchor_keeps_future_cohort_out_of_the_catchup_band() {
        // The reclaimer's clock frame.
        let now = 1_000_000;
        // Origin minted the keepalive 300 s out on ITS clock, which is 45 s BEHIND
        // the reclaimer → the reclaimer received it with skew_offset = +45_000, and
        // the raw fire_at (origin frame) reads as `now - 45_000 + 300_000` once we
        // subtract the offset back out. Model the raw (pre-correction) fire_at:
        let skew = 45_000; // receiver_now − origin_now
        let raw_fire_at = now - skew + 300_000; // origin-frame deadline as stored
        // Before correction this is `now + 255_000` → looks 45 s "closer" but still
        // future; a LARGER skew would flip it past-due. Use a skew big enough to
        // flip it: origin minted it only 30 s out.
        let raw_fire_at_flip = now - skew + 30_000; // = now - 15_000 → PAST-DUE raw!
        let mut timers = vec![keepalive(raw_fire_at_flip)];
        // l_max computed over the CORRECTED deadline (as reclaim_all now does):
        // corrected = raw + skew = now + 30_000 (future) → not past-due → l_max 0.
        let smoothing = Some(Smoothing { now_ms: now, l_max: 0, speedup: 10, cap_ms: None });
        sanitize_restored_timers(&mut timers, "w1|c|c", now, Some(skew), 300_000, smoothing);
        let fa = timers[0].fire_at;
        assert!(
            fa >= now,
            "corrected future keepalive is NOT crushed to a past-due catch-up slot: {fa} < {now}",
        );
        assert!(
            fa <= now + 30_000,
            "de-correlated within [now, corrected deadline], not the raw past-due frame: {fa}",
        );
        // Control: the SAME raw timers WITHOUT re-anchoring (offset None) would be
        // seen past-due and smoothed toward the catch-up band at `now`.
        let mut raw_timers = vec![keepalive(raw_fire_at_flip)];
        smooth_keepalives(&mut raw_timers, "w1|c|c", now, 15_000, 10, None);
        assert!(
            raw_timers[0].fire_at <= now,
            "uncorrected: the future keepalive IS mis-compressed to the catch-up band",
        );
        let _ = raw_fire_at;
    }

    /// Test #4 — the defensive floor. When the offset is UNKNOWN (`None`, a path
    /// that could not re-anchor), a keepalive past-due by ≥ 1× interval is re-based
    /// to within one interval of `now` (not fired immediately, which would race a
    /// failed-over transaction). Deterministic per (call_ref, timer id).
    #[test]
    fn defensive_floor_rebases_deep_past_due_keepalive_within_one_interval() {
        let now = 1_000_000;
        let interval = 300_000;
        // Past-due by 2× interval — deep skew/backlog pathology.
        let make = || vec![keepalive(now - 2 * interval)];

        let mut t = make();
        // Unknown offset (None) → floor engages; no smoothing.
        sanitize_restored_timers(&mut t, "w1|d|d", now, None, interval, None);
        let fa = t[0].fire_at;
        assert!(
            (now..now + interval).contains(&fa),
            "deep-past-due keepalive re-based into [now, now+interval): {fa}",
        );
        assert!(fa > now, "not fired immediately at now (which would race the failed-over txn)");

        // Determinism: same (call_ref, id) → same slot (idempotent reboot re-pass).
        let mut t2 = make();
        sanitize_restored_timers(&mut t2, "w1|d|d", now, None, interval, None);
        assert_eq!(t2[0].fire_at, fa, "floor is deterministic per call_ref/id");
        // Distinct call_ref → (very likely) a different slot.
        let mut t3 = make();
        sanitize_restored_timers(&mut t3, "w1|different|x", now, None, interval, None);
        // (Not asserting inequality hard — hash collisions are possible — but the
        // slot must still be within the interval.)
        assert!((now..now + interval).contains(&t3[0].fire_at));

        // KNOWN offset (Some, even 0) → floor SKIPPED: a well-anchored past-due
        // keepalive fires promptly (single-clock transparency).
        let mut t4 = make();
        sanitize_restored_timers(&mut t4, "w1|d|d", now, Some(0), interval, None);
        assert_eq!(
            t4[0].fire_at,
            now - 2 * interval,
            "known offset trusts the deadline — past-due fires promptly, floor does NOT engage",
        );
    }

    /// Test #2 — SetupTimeout (a non-keepalive policy deadline) under skew. A
    /// ringing call partway through its setup window fails over; the restored
    /// SetupTimeout must land at its TRUE remaining time in the takeover node's
    /// clock frame, NOT reaped early (skew-ahead) nor extended by the skew
    /// (skew-behind). The seam re-anchors ALL timer classes, so this holds for the
    /// SetupTimeout ledger timer exactly as for the keepalive — the class that got
    /// NO restore hygiene at all before this change.
    #[test]
    fn setup_timeout_is_reanchored_not_reaped_early_or_extended() {
        // Origin minted SetupTimeout 150 s out at ring-start; the call is 100 s in,
        // so 50 s of setup window remains (origin-frame fire_at = origin_ring + 150).
        // Model the restored entry as it arrives on the takeover node, whose `now`
        // is 100 s past the (origin-frame) ring start.
        let setup = |fire_at: i64| TimerEntry {
            id: format!("{:?}", TimerType::SetupTimeout),
            timer_type: TimerType::SetupTimeout,
            fire_at,
            leg_id: None,
        };

        // ── skew-AHEAD (+40 s): the takeover node's clock is 40 s ahead of the
        //    origin. Raw fire_at (origin frame) = now_local − 40_000 + 50_000 (still
        //    50 s of true window). WITHOUT re-anchor it would read 10 s out (reaped
        //    40 s early); WITH +40 s re-anchor it lands at the true 50 s. ──────────
        let now = 1_000_000;
        let skew_ahead = 40_000;
        let raw_fire_at = now - skew_ahead + 50_000; // origin-frame deadline
        let mut t = vec![setup(raw_fire_at)];
        sanitize_restored_timers(&mut t, "w1|s|s", now, Some(skew_ahead), 300_000, None);
        assert_eq!(
            t[0].fire_at,
            now + 50_000,
            "skew-ahead: SetupTimeout lands at the TRUE remaining 50 s, not reaped 40 s early",
        );

        // ── skew-BEHIND (−40 s): the takeover node's clock is 40 s behind the
        //    origin. Raw fire_at (origin frame) = now_local + 40_000 + 50_000.
        //    WITHOUT re-anchor it would read 90 s out (extended by the skew); WITH
        //    −40 s re-anchor it lands at the true 50 s. ─────────────────────────────
        let skew_behind = -40_000;
        let raw_fire_at_b = now - skew_behind + 50_000; // = now + 90_000 (origin frame)
        let mut tb = vec![setup(raw_fire_at_b)];
        sanitize_restored_timers(&mut tb, "w1|s|s", now, Some(skew_behind), 300_000, None);
        assert_eq!(
            tb[0].fire_at,
            now + 50_000,
            "skew-behind: SetupTimeout lands at the TRUE remaining 50 s, not extended by 40 s",
        );
    }

    /// The re-anchor itself: a known offset shifts every timer's absolute deadline
    /// into the local clock frame; a sub-deadband offset is a no-op (latency, not
    /// skew — keeps the single-clock harness transparent).
    #[test]
    fn reanchor_applies_above_deadband_and_ignores_below() {
        let mut t = vec![keepalive(1_000_000), keepalive_timeout("a", 900_000)];
        reanchor_timers(&mut t, 30_000); // +30 s real skew
        assert_eq!(t[0].fire_at, 1_030_000);
        assert_eq!(t[1].fire_at, 930_000, "ALL timer classes re-anchored, not just keepalive");

        let mut small = vec![keepalive(1_000_000)];
        reanchor_timers(&mut small, 200); // 200 ms — below the 1 s deadband
        assert_eq!(small[0].fire_at, 1_000_000, "sub-deadband latency offset is a no-op");
    }
}
