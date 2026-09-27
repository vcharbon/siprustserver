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
//! three CORE_LAYER rules (`reaper-stale`, `reaper-fatal-error`,
//! `reaper-overflow`). Only the
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
use call::{
    ByeDisposition, Call, CallModelState, CdrEvent, CdrEventType, LegState, TerminationCause,
};
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
/// Overflow verdict: the call's dispatch overflow reached its ceiling — more
/// must-run events arrive than the call consumes. Handled by the CORE_LAYER
/// `reaper-overflow` rule, which tears the live legs down on the wire;
/// re-sent each sweep on its own escalation ladder while the call stays
/// Active.
pub const OUTCOME_OVERFLOW: &str = "overflow";
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
    /// Stale-verdict attempts per call_ref (the abort/discharge escalation).
    attempts: Arc<Mutex<HashMap<String, u32>>>,
    /// Overflow-verdict attempts per call_ref, a ladder of their own so a call
    /// both stale and condemned climbs one rung per sweep on each.
    condemned_attempts: Arc<Mutex<HashMap<String, u32>>>,
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
            sweep_interval_ms: sweep_interval_sec.max(1) as u64 * 1000,
            idle_max_ms,
            strikes: Arc::new(Mutex::new(HashMap::new())),
            attempts: Arc::new(Mutex::new(HashMap::new())),
            condemned: Arc::new(Mutex::new(HashSet::new())),
            condemned_attempts: Arc::new(Mutex::new(HashMap::new())),
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

    /// The dispatcher-facing overflow hook: a call flooded past its overflow
    /// ceiling is condemned — one `overflow` verdict now, then one per sweep
    /// with the stale escalation (abort, discharge) until it is gone. Once
    /// per call, so a flood sends one verdict, not one per refused job.
    pub fn overflow_hook(&self) -> crate::dispatch::OverflowHook {
        let this = self.clone();
        Arc::new(move |call_ref: &str| {
            if this.enabled && this.condemned.lock().unwrap().insert(call_ref.to_string()) {
                this.send_verdict(call_ref, OUTCOME_OVERFLOW, serde_json::json!({}));
            }
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

    /// One sweep pass: emit verdicts for stale candidates, then for condemned
    /// ones, at most [`MAX_VERDICTS_PER_SWEEP`] in all; escalate undelivered
    /// ones; prune dead escalation state. Sync + lock-light.
    fn sweep_once(&self, state: &CallState, dispatcher: &PerCallDispatcher, now_ms: i64) {
        self.metrics.bump_reaper_sweep();
        let mut budget = MAX_VERDICTS_PER_SWEEP;
        for (call_ref, watermark) in
            state.stale_candidates(now_ms, self.idle_max_ms).into_iter().take(budget)
        {
            budget -= 1;
            // Idempotent re-send: a queue-full drop is simply retried next
            // sweep — the reaper never assumes delivery. The verdict carries
            // the observed stamp; `process` discards it if the stamp moved
            // (X5), so a call that revived in the meantime is untouched.
            self.climb(&self.attempts, &call_ref, dispatcher, || {
                (OUTCOME_STALE, serde_json::json!({ "watermark": watermark }))
            });
        }
        // A condemned call that left Active has its teardown under way, bounded
        // by `TerminatingTimeout`: it needs no further verdict.
        let condemned: Vec<String> = {
            let mut set = self.condemned.lock().unwrap();
            set.retain(|r| state.peek(r).is_some_and(|c| c.state == CallModelState::Active));
            set.iter().cloned().collect()
        };
        for call_ref in condemned.into_iter().take(budget) {
            self.climb(&self.condemned_attempts, &call_ref, dispatcher, || {
                (OUTCOME_OVERFLOW, serde_json::json!({}))
            });
        }
        // Prune escalation state for calls no longer resident (normal
        // termination, reap completed, self-release) — bounded by live calls.
        self.strikes.lock().unwrap().retain(|r, _| state.contains(r));
        self.attempts.lock().unwrap().retain(|r, _| state.contains(r));
        self.condemned_attempts.lock().unwrap().retain(|r, _| state.contains(r));
    }

    /// One rung of `call_ref`'s escalation on `ladder`: the verdict `verdict`
    /// builds, then — its verdicts not taking effect — the in-flight body
    /// aborted (a hung body holds the worker and the per-call lock; dropping
    /// it lets the queued verdict run), then the discharge that bypasses the
    /// rules.
    fn climb(
        &self,
        ladder: &Mutex<HashMap<String, u32>>,
        call_ref: &str,
        dispatcher: &PerCallDispatcher,
        verdict: impl FnOnce() -> (&'static str, serde_json::Value),
    ) {
        let attempt = {
            let mut attempts = ladder.lock().unwrap();
            let a = attempts.entry(call_ref.to_string()).or_insert(0);
            *a += 1;
            *a
        };
        if attempt > ESCALATE_DISCHARGE_AFTER {
            self.send_verdict(call_ref, OUTCOME_DISCHARGE, serde_json::json!({}));
            return;
        }
        if attempt > ESCALATE_ABORT_AFTER {
            dispatcher.abort_in_flight(call_ref);
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
        let resolved = match leg.bye_disposition {
            None => leg.state == LegState::Trying,
            Some(b) => b.is_terminal(),
        };
        if !resolved {
            leg.bye_disposition = Some(ByeDisposition::ByeTimeout);
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
mod tests {
    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::router::test_support;
    use crate::store::InMemoryCallStore;

    /// The outcomes of the verdicts sent so far.
    fn outcomes(rx: &mut mpsc::UnboundedReceiver<CallEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(CallEvent::InternalEvent { outcome, .. }) = rx.try_recv() {
            out.push(outcome);
        }
        out
    }

    /// A call flooded past its dispatch overflow is condemned once however
    /// many refusals report it, then swept every pass with the stale ladder
    /// — the in-flight body aborted, then the discharge — until it is gone.
    #[tokio::test]
    async fn an_overflowing_call_is_condemned_once_and_swept_until_gone() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let metrics = B2buaMetrics::new();
        let reaper = Reaper::new(true, 1, 3_600_000, tx, metrics.clone());
        let state = CallState::new(Arc::new(InMemoryCallStore::new()), "w0", metrics.clone());
        let src = "10.0.0.9:5060".parse().unwrap();
        let invite = test_support::invite("w0", "w1", "flood");
        let call_ref = state.create(build_initial_call(
            &invite,
            src,
            &B2buaConfig::default(),
            &sip_txn::IdGen::seeded(1),
            0,
        ));
        let dispatcher = PerCallDispatcher::new(1, 1, 8, metrics.clone());

        let hook = reaper.overflow_hook();
        hook(&call_ref);
        hook(&call_ref);
        assert_eq!(outcomes(&mut rx), vec![OUTCOME_OVERFLOW], "one verdict per condemned call");

        for _ in 0..5 {
            reaper.maybe_sweep(&state, &dispatcher, 0);
        }
        assert_eq!(
            outcomes(&mut rx),
            vec![
                OUTCOME_OVERFLOW,
                OUTCOME_OVERFLOW,
                OUTCOME_OVERFLOW,
                OUTCOME_OVERFLOW,
                OUTCOME_DISCHARGE
            ]
        );

        state.remove(&call_ref);
        reaper.maybe_sweep(&state, &dispatcher, 0);
        assert!(outcomes(&mut rx).is_empty(), "a gone call is no longer swept");
        hook(&call_ref);
        assert_eq!(outcomes(&mut rx), vec![OUTCOME_OVERFLOW], "its condemnation was pruned");
    }

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

    fn condemned_rig(
        calls: usize,
    ) -> (Reaper, mpsc::UnboundedReceiver<CallEvent>, CallState, Vec<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let metrics = B2buaMetrics::new();
        let reaper = Reaper::new(true, 1, 60_000, tx, metrics.clone());
        let state = CallState::new(Arc::new(InMemoryCallStore::new()), "w0", metrics);
        let src = "10.0.0.9:5060".parse().unwrap();
        let refs = (0..calls)
            .map(|n| {
                let invite = test_support::invite("w0", "w1", &format!("flood{n}"));
                state.create(build_initial_call(&invite, src, &B2buaConfig::default(), 0))
            })
            .collect();
        (reaper, rx, state, refs)
    }

    /// A sweep sends at most `MAX_VERDICTS_PER_SWEEP` verdicts, condemned
    /// calls included.
    #[tokio::test]
    async fn the_condemned_loop_shares_the_sweep_budget() {
        let (reaper, mut rx, state, refs) = condemned_rig(MAX_VERDICTS_PER_SWEEP + 6);
        let hook = reaper.overflow_hook();
        for r in &refs {
            hook(r);
        }
        let _ = outcomes(&mut rx);
        let dispatcher = PerCallDispatcher::new(1, 1, 8, B2buaMetrics::new());
        reaper.maybe_sweep(&state, &dispatcher, 0);
        assert_eq!(outcomes(&mut rx).len(), MAX_VERDICTS_PER_SWEEP);
    }

    /// A call both stale and condemned climbs each ladder one rung per sweep:
    /// three sweeps reach neither discharge.
    #[tokio::test]
    async fn stale_and_overflow_verdicts_keep_their_own_ladders() {
        let (reaper, mut rx, state, refs) = condemned_rig(1);
        reaper.overflow_hook()(&refs[0]);
        let _ = outcomes(&mut rx);
        let dispatcher = PerCallDispatcher::new(1, 1, 8, B2buaMetrics::new());
        let far = i64::MAX / 2;
        for _ in 0..3 {
            reaper.maybe_sweep(&state, &dispatcher, far);
        }
        let sent = outcomes(&mut rx);
        assert_eq!(sent.len(), 6, "{sent:?}");
        assert!(!sent.iter().any(|o| o == OUTCOME_DISCHARGE), "{sent:?}");
    }

    /// A condemned call whose teardown has begun gets no further verdict.
    #[tokio::test]
    async fn a_condemned_call_that_left_active_is_not_swept() {
        let (reaper, mut rx, state, refs) = condemned_rig(1);
        reaper.overflow_hook()(&refs[0]);
        let _ = outcomes(&mut rx);
        let mut call = state.peek(&refs[0]).unwrap();
        call.state = CallModelState::Terminating;
        state.update(call);
        let dispatcher = PerCallDispatcher::new(1, 1, 8, B2buaMetrics::new());
        reaper.maybe_sweep(&state, &dispatcher, 0);
        assert!(outcomes(&mut rx).is_empty());
    }
}
