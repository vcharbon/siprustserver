//! The **call reaper** (ADR-0020) — the in-process backstop for the promise
//! *"every call admitted to the live map is released exactly once, through the
//! existing `release_call` + invariant path, with exactly one CDR."*
//!
//! Failure classes covered (in-process only; hard-crash windows are out of
//! scope — HA reclaim owns post-crash recovery):
//! - **handler panic** — the dispatcher's failure hook reports it;
//! - **dropped event** (a queue-full BYE) / **lost timer** (incl. a lost
//!   `TerminatingTimeout`) / **wedged Terminating** — the **last-touched
//!   stamp** freezes and the sweep notices.
//!
//! There is **no second teardown vocabulary**: every reaper verdict is a
//! synthetic `CallEvent::InternalEvent { topic: "reaper" }` down the existing
//! re-entry channel, through the per-call FIFO and per-call lock, handled by
//! four CORE_LAYER rules (`reaper-stale`, `reaper-fatal-error`,
//! `reaper-overflow`, `reaper-message-cap`). Only the
//! strike-2 **discharge** bypasses the *rules* stage (rules are the thing that
//! failed); it still runs `finalize → enforce → process_result` — the
//! [`ObligationSet`](crate::obligations::ObligationSet) derivation, the CDR,
//! and the propagated delete all ride the one funnel.
//!
//! **Scope** (ADR-0020 X3): primary-served live calls + re-hydrated calls.
//! `CallState::stale_candidates` excludes acting-backup takeover copies
//! (self-release stays push-based on `CallQuiesced`, per ADR-0014) and `bak:`
//! Elements are structurally absent from the live map. The sweep adds no
//! time-based input to HA reconciliation.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use call::helpers::record_termination;
use call::{Call, CallModelState, CdrEvent, CdrEventType, TerminationCause};
use tokio::sync::mpsc;

use crate::dispatch::{HandlerFailure, InFlight};
use crate::effects::HandlerResult;
use crate::metrics::B2buaMetrics;
use crate::poison::{locked, REAPER_LEDGERS};
use crate::store::{CallState, Reconciled};
use b2bua_sdk::event::CallEvent;

/// One of the reaper's ledgers ([`REAPER_LEDGERS`]).
fn ledger<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    locked(m, REAPER_LEDGERS)
}

pub use crate::reaper_verdict::{
    OUTCOME_DISCHARGE, OUTCOME_FATAL, OUTCOME_MESSAGE_CAP, OUTCOME_OVERFLOW, OUTCOME_STALE,
    REAPER_TOPIC,
};

/// Sweep pacing: at most this many verdicts per tick, so a mass-wedge (or a
/// mis-tuned idle threshold) becomes a paced drain, never a teardown storm.
const MAX_VERDICTS_PER_SWEEP: usize = 64;
/// Stale verdicts delivered-but-ineffective before the sweep aborts the
/// in-flight body (a hung handler holds the worker AND the per-call lock; the
/// abort drops the body — releasing the lock — and the queued verdict then
/// flows through the normal funnel).
const ESCALATE_ABORT_AFTER: u32 = 2;
/// Of each sweep's verdicts, how many condemned calls may claim before stale
/// ones: a wave of stale calls cannot starve them.
const CONDEMNED_SHARE_PER_SWEEP: usize = MAX_VERDICTS_PER_SWEEP / 4;
/// Verdict attempts before bypassing the rules entirely (discharge).
const ESCALATE_DISCHARGE_AFTER: u32 = 4;

/// The reaper handle: owns the node-local escalation memory (strike + attempt
/// counts) and the verdict injection. Clone-cheap. Deliberately NOT an
/// obligation registry — it carries zero deallocation knowledge; losing it
/// (reboot) is safe because reclaim re-stamps and the sweep restarts
/// assessment from the snapshot.
#[derive(Clone)]
pub struct Reaper {
    enabled: bool,
    sweep_interval_ms: u64,
    idle_max_ms: i64,
    /// Panic/abort strikes per call_ref (the X6 two-strike ladder). Pruned
    /// against the live map each sweep so it cannot ratchet.
    strikes: Arc<Mutex<HashMap<String, u32>>>,
    /// Stale-verdict attempts per call_ref (the abort/discharge escalation).
    attempts: Arc<Mutex<HashMap<String, u32>>>,
    /// Overflow-verdict attempts per call_ref, a ladder of their own so a call
    /// both stale and condemned climbs one rung per sweep on each.
    condemned_attempts: Arc<Mutex<HashMap<String, u32>>>,
    /// Calls whose in-flight body the overflow ladder aborted and whose
    /// `Aborted` report is still due: that report is no handler failure (the
    /// call's own overflow verdict is queued behind the body), so it strikes
    /// nothing.
    overflow_aborts: Arc<Mutex<HashSet<String>>>,
    /// Calls that crossed their lifetime message cap and were sent their one
    /// verdict. Pruned against the live map each sweep.
    capped: Arc<Mutex<HashSet<String>>>,
    /// Calls whose dispatch overflow reached its ceiling: swept while Active.
    /// Pruned each sweep.
    condemned: Arc<Mutex<HashSet<String>>>,
    reentry_tx: mpsc::UnboundedSender<CallEvent>,
    metrics: B2buaMetrics,
}

impl Reaper {
    /// Build the handle. `idle_max_ms` / `sweep_interval` come pre-resolved
    /// from `B2buaConfig` (`reaper_idle_max_ms()`); `reentry_tx` is the
    /// router's existing re-entrant event channel.
    pub fn new(
        enabled: bool,
        sweep_interval_sec: i64,
        idle_max_ms: i64,
        reentry_tx: mpsc::UnboundedSender<CallEvent>,
        metrics: B2buaMetrics,
    ) -> Self {
        Self {
            enabled,
            sweep_interval_ms: u64::try_from(sweep_interval_sec.max(1))
                .unwrap_or(1)
                .saturating_mul(1000),
            idle_max_ms,
            strikes: Arc::new(Mutex::new(HashMap::new())),
            attempts: Arc::new(Mutex::new(HashMap::new())),
            condemned: Arc::new(Mutex::new(HashSet::new())),
            condemned_attempts: Arc::new(Mutex::new(HashMap::new())),
            overflow_aborts: Arc::new(Mutex::new(HashSet::new())),
            capped: Arc::new(Mutex::new(HashSet::new())),
            reentry_tx,
            metrics,
        }
    }

    /// The dispatcher-facing failure hook (X6): strike 1 → a `fatal-error`
    /// verdict through the normal rules; strike ≥ 2 (the rules path itself
    /// failed) → `discharge`. Aborts count as strikes too — an aborted body
    /// was hung, so its call needs the same resolution — except one the
    /// overflow ladder asked for: its call's teardown is already queued, and a
    /// strike would force the legs that teardown is ending on the wire.
    pub fn failure_hook(&self) -> crate::dispatch::FailureHook {
        let this = self.clone();
        Arc::new(move |call_ref: &str, failure: HandlerFailure| {
            if !this.enabled {
                return;
            }
            if failure == HandlerFailure::Aborted && ledger(&this.overflow_aborts).remove(call_ref)
            {
                return;
            }
            let strike = {
                let mut strikes = ledger(&this.strikes);
                let s = strikes.entry(call_ref.to_string()).or_insert(0);
                *s += 1;
                *s
            };
            let outcome = if strike == 1 { OUTCOME_FATAL } else { OUTCOME_DISCHARGE };
            this.send_verdict(call_ref, outcome, serde_json::json!({ "strike": strike }));
        })
    }

    /// Hears a call whose dispatch overflow reached its ceiling: it is
    /// condemned — one `overflow` verdict now, then one per sweep with the
    /// stale escalation (abort, discharge) until it is gone. Once per call,
    /// so a flood sends one verdict, not one per refused event.
    pub fn on_overflow_ceiling(&self, call_ref: &str) {
        if self.enabled && ledger(&self.condemned).insert(call_ref.to_string()) {
            self.send_verdict(call_ref, OUTCOME_OVERFLOW, serde_json::json!({}));
        }
    }

    /// Hears a call that crossed its lifetime message cap: one
    /// `message-cap` verdict. The verdict is admitted past every bound and
    /// the call's requests are refused from then on, so one is enough.
    pub fn on_lifetime_cap(&self, call_ref: &str) {
        if self.enabled && ledger(&self.capped).insert(call_ref.to_string()) {
            self.send_verdict(call_ref, OUTCOME_MESSAGE_CAP, serde_json::json!({}));
        }
    }

    /// One paced sweep pass, gated on `enabled` (a no-op for a disabled reaper).
    /// Driven per tick by the single sweep task `b2bua_core` spawns — which also
    /// drives the Model-Y backup-durable reap, so the two periodic concerns share
    /// one `tokio::time::interval` (deterministic under `start_paused` +
    /// `Harness::advance`) with an explicit ordering instead of two racing timers.
    ///
    /// Every pass first reconciles the store's derived entries with its call
    /// map ([`CallState::reconcile`]), counting each correction by kind,
    /// whether the reaper is enabled or not: the stamps it reads are among
    /// them.
    pub fn maybe_sweep(&self, state: &CallState, in_flight: &InFlight, now_ms: i64) {
        self.count_reconciled(state.reconcile(now_ms));
        if self.enabled {
            self.sweep_once(state, in_flight, now_ms);
        }
    }

    /// Count one reconcile pass's corrections, logging a pass that made any.
    fn count_reconciled(&self, fixed: Reconciled) {
        if fixed.is_clean() {
            return;
        }
        tracing::warn!(
            node = observe::node(),
            ?fixed,
            "the store reconciled entries a cut write left"
        );
        let m = &self.metrics;
        for (n, bump) in [
            (fixed.orphan_stamps, B2buaMetrics::bump_store_reconciled_orphan_stamp as fn(&_)),
            (fixed.missing_stamps, B2buaMetrics::bump_store_reconciled_missing_stamp),
            (fixed.orphan_index, B2buaMetrics::bump_store_reconciled_orphan_index),
            (fixed.missing_index, B2buaMetrics::bump_store_reconciled_missing_index),
            (fixed.orphan_takeover, B2buaMetrics::bump_store_reconciled_orphan_takeover),
        ] {
            for _ in 0..n {
                bump(m);
            }
        }
    }

    /// The shared sweep cadence (ms) — `b2bua_core` reads it to size the one task.
    pub fn sweep_interval_ms(&self) -> u64 {
        self.sweep_interval_ms
    }

    /// One sweep pass: emit verdicts for stale candidates, then for condemned
    /// ones, at most [`MAX_VERDICTS_PER_SWEEP`] in all; escalate undelivered
    /// ones; prune dead escalation state. Sync + lock-light.
    fn sweep_once(&self, state: &CallState, in_flight: &InFlight, now_ms: i64) {
        self.metrics.bump_reaper_sweep();
        // A condemned call that left Active has its teardown under way, bounded
        // by `TerminatingTimeout`: it needs no further verdict. A takeover copy
        // is its self-release's (ADR-0020 X3).
        let condemned: Vec<String> = {
            let mut set = ledger(&self.condemned);
            set.retain(|r| {
                state.model_state(r) == Some(CallModelState::Active) && !state.is_takeover(r)
            });
            set.iter().cloned().collect()
        };
        // Condemned calls keep a share of the budget however many are stale.
        let reserved = condemned.len().min(CONDEMNED_SHARE_PER_SWEEP);
        let mut budget = MAX_VERDICTS_PER_SWEEP;
        for (call_ref, watermark) in state
            .stale_candidates(now_ms, self.idle_max_ms)
            .into_iter()
            .take(MAX_VERDICTS_PER_SWEEP - reserved)
        {
            budget -= 1;
            // Idempotent re-send: each sweep sends the verdict again while
            // the call stays stale — the reaper never assumes it took effect.
            // The verdict carries the observed stamp; `process` discards it
            // if the stamp moved (X5), so a call that revived in the meantime
            // is untouched.
            self.climb(&self.attempts, &call_ref, in_flight, false, || {
                (OUTCOME_STALE, serde_json::json!({ "watermark": watermark }))
            });
        }
        for call_ref in condemned.into_iter().take(budget) {
            self.climb(&self.condemned_attempts, &call_ref, in_flight, true, || {
                (OUTCOME_OVERFLOW, serde_json::json!({}))
            });
        }
        // Prune escalation state for calls no longer resident (normal
        // termination, reap completed, self-release) — bounded by live calls.
        ledger(&self.strikes).retain(|r, _| state.contains(r));
        ledger(&self.attempts).retain(|r, _| state.contains(r));
        ledger(&self.condemned_attempts).retain(|r, _| state.contains(r));
        ledger(&self.overflow_aborts).retain(|r| state.contains(r));
        ledger(&self.capped).retain(|r| state.contains(r));
    }

    /// One rung of `call_ref`'s escalation on `ladder`: the verdict `verdict`
    /// builds, then — its verdicts not taking effect — the in-flight body
    /// aborted (a hung body holds the worker and the per-call lock; dropping
    /// it lets the queued verdict run), then the discharge that bypasses the
    /// rules.
    /// `overflow`: the abort is the overflow ladder's own, and strikes
    /// nothing.
    fn climb(
        &self,
        ladder: &Mutex<HashMap<String, u32>>,
        call_ref: &str,
        in_flight: &InFlight,
        overflow: bool,
        verdict: impl FnOnce() -> (&'static str, serde_json::Value),
    ) {
        let attempt = {
            let mut attempts = ledger(ladder);
            let a = attempts.entry(call_ref.to_string()).or_insert(0);
            *a += 1;
            *a
        };
        if attempt > ESCALATE_DISCHARGE_AFTER {
            self.send_verdict(call_ref, OUTCOME_DISCHARGE, serde_json::json!({}));
            return;
        }
        if attempt > ESCALATE_ABORT_AFTER {
            if overflow {
                ledger(&self.overflow_aborts).insert(call_ref.to_string());
            }
            if !in_flight.abort(call_ref) && overflow {
                ledger(&self.overflow_aborts).remove(call_ref);
            }
        }
        let (outcome, payload) = verdict();
        self.send_verdict(call_ref, outcome, payload);
    }

    fn send_verdict(&self, call_ref: &str, outcome: &str, payload: serde_json::Value) {
        self.metrics.bump_reaper_verdict();
        // A closed channel (shutdown) drops the verdict — never panic.
        let _ = self.reentry_tx.send(CallEvent::InternalEvent {
            call_ref: call_ref.to_string(),
            topic: REAPER_TOPIC.to_string(),
            outcome: outcome.to_string(),
            payload,
            body: Vec::new(),
            // Confirmed by the call's liveness stamp, not by incarnation
            // (ADR-0020 X5).
            incarnation: None,
        });
    }
}

/// X5 confirm — check-then-act made safe, pure. `watermark` is the stamp the
/// sweep observed (stale verdicts only); `current` is the live ledger stamp
/// (`None` = the call is gone). A verdict applies iff:
/// - **stale**: the stamp is unchanged (the call was NOT touched since the
///   sweep observed it) — and the call is still resident. A gone call returns
///   `false`, which is also what prevents a late verdict from resurrecting it
///   from the replica store via on-demand reclaim.
/// - **fatal-error / discharge** (failure-hook verdicts, no watermark): the
///   call is still resident.
pub fn verdict_confirmed(outcome: &str, watermark: Option<i64>, current: Option<i64>) -> bool {
    match outcome {
        OUTCOME_STALE => matches!((watermark, current), (Some(w), Some(cur)) if cur == w),
        _ => current.is_some(),
    }
}

/// The strike-2 **discharge** (ADR-0020 X6): force the last persisted snapshot
/// terminal — every unresolved leg is abandoned ([`call::helpers::abandon_leg`]), one
/// reason-carrying `CdrEvent` is appended, the termination record names the
/// supervisor where nothing ended the call before, `state = Terminated` — and return
/// it with EMPTY effects: the caller runs the ordinary
/// `finalize → enforce → process_result`, so the CDR write, the limiter
/// release (owed by a counted call, derived by the `ObligationSet`), and the
/// final `RemoveCall → release_call(Terminated)` all come from the ONE
/// enforcement path. Pure, panic-free by construction (no rule code, no SDP,
/// no relay logic).
pub fn discharge_result(mut call: Call, now_ms: i64) -> HandlerResult {
    let a_leg_id = call.a_leg.leg_id.clone();
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        if !leg.is_resolved() {
            call::helpers::abandon_leg(leg);
        }
    }
    let call = call::helpers::add_cdr_event(
        call,
        CdrEvent {
            event_type: CdrEventType::Bye,
            timestamp: now_ms,
            leg_id: a_leg_id,
            status_code: None,
            reason: Some("reaper-discharge".to_string()),
            decision_ordinal: 0,
        },
    );
    let mut call = record_termination(call, now_ms, TerminationCause::Supervisor, None);
    call.state = CallModelState::Terminated;
    HandlerResult::new(call)
}

#[cfg(test)]
mod tests;
