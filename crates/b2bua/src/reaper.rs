//! The **call reaper** (ADR-0020) — the in-process backstop for the promise
//! *"every call admitted to the live map is released exactly once, through the
//! existing `release_call` + invariant path, with exactly one CDR."*
//!
//! Failure classes covered (in-process only; hard-crash windows are out of
//! scope — HA reclaim owns post-crash recovery):
//! - **handler panic** — the dispatcher's failure hook reports it (the
//!   pre-reaper swallow leaked the call forever with zero CDR);
//! - **dropped event** (a queue-full BYE) / **lost timer** (incl. a lost
//!   `TerminatingTimeout`) / **wedged Terminating** — the **last-touched
//!   stamp** freezes and the sweep notices.
//!
//! There is **no second teardown vocabulary**: every reaper verdict is a
//! synthetic `CallEvent::InternalEvent { topic: "reaper" }` down the existing
//! re-entry channel, through the per-call FIFO and per-call lock, handled by
//! two CORE_LAYER rules (`reaper-stale`, `reaper-fatal-error`). Only the
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use call::{ByeDisposition, Call, CallModelState, CdrEvent, CdrEventType, LegState};
use tokio::sync::mpsc;

use crate::dispatch::{HandlerFailure, PerCallDispatcher};
use crate::effects::HandlerResult;
use crate::event::CallEvent;
use crate::metrics::B2buaMetrics;
use crate::store::CallState;

/// The synthetic-event topic every reaper verdict carries.
pub const REAPER_TOPIC: &str = "reaper";
/// Sweep verdict: the call's last-touched stamp is older than the idle
/// threshold. Handled by the CORE_LAYER `reaper-stale` rule. Carries the
/// observed stamp as `payload.watermark` (the X5 confirm input).
pub const OUTCOME_STALE: &str = "stale";
/// Strike-1 verdict: a handler body for this call panicked. Handled by the
/// CORE_LAYER `reaper-fatal-error` rule.
pub const OUTCOME_FATAL: &str = "fatal-error";
/// Strike-2 verdict: the rules path itself failed (a second panic, or stale
/// verdicts that never took effect). Deliberately has NO rule — the router's
/// discharge branch forces the last persisted snapshot terminal and runs it
/// through `finalize → enforce → process_result` directly.
pub const OUTCOME_DISCHARGE: &str = "discharge";

/// Sweep pacing: at most this many verdicts per tick, so a mass-wedge (or a
/// mis-tuned idle threshold) becomes a paced drain, never a teardown storm —
/// the keepalive-shed lesson.
const MAX_VERDICTS_PER_SWEEP: usize = 64;
/// Stale verdicts delivered-but-ineffective before the sweep aborts the
/// in-flight body (a hung handler holds the worker AND the per-call lock; the
/// abort drops the body — releasing the lock — and the queued verdict then
/// flows through the normal funnel).
const ESCALATE_ABORT_AFTER: u32 = 2;
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
    /// Sweep-verdict attempts per call_ref (the abort/discharge escalation).
    attempts: Arc<Mutex<HashMap<String, u32>>>,
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
            sweep_interval_ms: sweep_interval_sec.max(1) as u64 * 1000,
            idle_max_ms,
            strikes: Arc::new(Mutex::new(HashMap::new())),
            attempts: Arc::new(Mutex::new(HashMap::new())),
            reentry_tx,
            metrics,
        }
    }

    /// The dispatcher-facing failure hook (X6): strike 1 → a `fatal-error`
    /// verdict through the normal rules; strike ≥ 2 (the rules path itself
    /// failed) → `discharge`. Aborts count as strikes too — an aborted body
    /// was hung, so its call needs the same forced resolution.
    pub fn failure_hook(&self) -> crate::dispatch::FailureHook {
        let this = self.clone();
        Arc::new(move |call_ref: &str, _failure: HandlerFailure| {
            if !this.enabled {
                return;
            }
            let strike = {
                let mut strikes = this.strikes.lock().unwrap();
                let s = strikes.entry(call_ref.to_string()).or_insert(0);
                *s += 1;
                *s
            };
            let outcome = if strike == 1 { OUTCOME_FATAL } else { OUTCOME_DISCHARGE };
            this.send_verdict(call_ref, outcome, serde_json::json!({ "strike": strike }));
        })
    }

    /// One paced sweep pass, gated on `enabled` (a no-op for a disabled reaper).
    /// Driven per tick by the single sweep task `b2bua_core` spawns — which also
    /// drives the Model-Y backup-durable reap, so the two periodic concerns share
    /// one `tokio::time::interval` (deterministic under `start_paused` +
    /// `Harness::advance`) with an explicit ordering instead of two racing timers.
    pub fn maybe_sweep(&self, state: &CallState, dispatcher: &PerCallDispatcher, now_ms: i64) {
        if self.enabled {
            self.sweep_once(state, dispatcher, now_ms);
        }
    }

    /// The shared sweep cadence (ms) — `b2bua_core` reads it to size the one task.
    pub fn sweep_interval_ms(&self) -> u64 {
        self.sweep_interval_ms
    }

    /// One sweep pass: emit verdicts for stale candidates (paced), escalate
    /// undelivered ones, prune dead escalation state. Sync + lock-light.
    fn sweep_once(&self, state: &CallState, dispatcher: &PerCallDispatcher, now_ms: i64) {
        self.metrics.bump_reaper_sweep();
        for (call_ref, watermark) in state
            .stale_candidates(now_ms, self.idle_max_ms)
            .into_iter()
            .take(MAX_VERDICTS_PER_SWEEP)
        {
            let attempt = {
                let mut attempts = self.attempts.lock().unwrap();
                let a = attempts.entry(call_ref.clone()).or_insert(0);
                *a += 1;
                *a
            };
            if attempt > ESCALATE_DISCHARGE_AFTER {
                // The stale verdicts never took effect (rules path broken /
                // verdicts repeatedly dropped) — bypass the rules.
                self.send_verdict(&call_ref, OUTCOME_DISCHARGE, serde_json::json!({}));
                continue;
            }
            if attempt > ESCALATE_ABORT_AFTER {
                // A hung body holds the worker + per-call lock; aborting drops
                // it (the lock guard releases) so the queued verdict can run.
                dispatcher.abort_in_flight(&call_ref);
            }
            // Idempotent re-send: a queue-full drop is simply retried next
            // sweep — the reaper never assumes delivery. The verdict carries
            // the observed stamp; `process` discards it if the stamp moved
            // (X5), so a call that revived in the meantime is untouched.
            self.send_verdict(
                &call_ref,
                OUTCOME_STALE,
                serde_json::json!({ "watermark": watermark }),
            );
        }
        // Prune escalation state for calls no longer resident (normal
        // termination, reap completed, self-release) — bounded by live calls.
        self.strikes.lock().unwrap().retain(|r, _| state.contains(r));
        self.attempts.lock().unwrap().retain(|r, _| state.contains(r));
    }

    fn send_verdict(&self, call_ref: &str, outcome: &str, payload: serde_json::Value) {
        self.metrics.bump_reaper_verdict();
        // A closed channel (shutdown) drops the verdict — never panic.
        let _ = self.reentry_tx.send(CallEvent::InternalEvent {
            call_ref: call_ref.to_string(),
            topic: REAPER_TOPIC.to_string(),
            outcome: outcome.to_string(),
            payload,
        });
    }
}

/// Is `event` a reaper verdict? (the router's guard/discharge gate)
pub fn is_reaper_event(event: &CallEvent) -> bool {
    matches!(event, CallEvent::InternalEvent { topic, .. } if topic == REAPER_TOPIC)
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
/// terminal — every unresolved leg gets `ByeDisposition::ByeTimeout`, one
/// reason-carrying `CdrEvent` is appended, `state = Terminated` — and return
/// it with EMPTY effects: the caller runs the ordinary
/// `finalize → enforce → process_result`, so the CDR write, the limiter
/// decrements (derived from `limiter_entries` by the `ObligationSet`), and the
/// final `RemoveCall → release_call(Terminated)` all come from the ONE
/// enforcement path. Pure, panic-free by construction (no rule code, no SDP,
/// no relay logic).
pub fn discharge_result(mut call: Call, now_ms: i64) -> HandlerResult {
    let a_leg_id = call.a_leg.leg_id.clone();
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        let resolved = match leg.bye_disposition {
            None => leg.state == LegState::Trying,
            Some(b) => b.is_terminal(),
        };
        if !resolved {
            leg.bye_disposition = Some(ByeDisposition::ByeTimeout);
        }
    }
    call.cdr_events.push(CdrEvent {
        event_type: CdrEventType::Bye,
        timestamp: now_ms,
        leg_id: a_leg_id,
        status_code: None,
        reason: Some("reaper-discharge".to_string()),
    });
    call.state = CallModelState::Terminated;
    HandlerResult::new(call)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_confirm_matrix() {
        // Stale verdict: applies only on an exact stamp match.
        assert!(verdict_confirmed(OUTCOME_STALE, Some(5), Some(5)));
        assert!(!verdict_confirmed(OUTCOME_STALE, Some(5), Some(9)), "touched since the sweep");
        assert!(!verdict_confirmed(OUTCOME_STALE, Some(5), None), "released — never resurrect");
        assert!(!verdict_confirmed(OUTCOME_STALE, None, Some(5)), "malformed verdict");
        // Failure-hook verdicts: apply iff the call is still resident.
        assert!(verdict_confirmed(OUTCOME_FATAL, None, Some(5)));
        assert!(!verdict_confirmed(OUTCOME_FATAL, None, None));
        assert!(verdict_confirmed(OUTCOME_DISCHARGE, None, Some(5)));
        assert!(!verdict_confirmed(OUTCOME_DISCHARGE, None, None));
    }
}
