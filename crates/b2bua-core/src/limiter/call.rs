//! The call's side of its limiter: the refresh it keeps armed and the
//! release it owes.
//!
//! [`arm_refresh`] is the one arming of a counted call's `LimiterRefresh`,
//! run on every turn. The refresh turn marks the call due on the worker's
//! refresh batch, never waiting on the limiter; the batch's answer comes back
//! as a re-entrant event and is applied on the call's own turn (ADR-0040).
//! `LimiterObligations` is the release a call owes at termination.

use call::{Call, CallModelState, TimerKey, TimerType};

use crate::effects::{HandlerEffects, HandlerResult, SoftBoundedEffect};
use crate::obligations::{Obligation, ObligationKind};

/// A counted Active call carries a live `LimiterRefresh` timer due within one
/// refresh `period` of the learnt lease. A missing entry (the turn's own
/// admit, the refresh turn whose entry fired) and one due later than
/// `now_ms + period` (armed under a longer period) are re-armed at
/// `now_ms + period`; a past-due one (a copy materialised with a stale
/// ledger, a fire the per-call queue dropped) is re-armed due now, so the
/// refresh it owes runs at once. Each on the record and as a `ScheduleTimer`.
pub fn arm_refresh(
    mut result: HandlerResult,
    now_ms: i64,
    period: std::time::Duration,
) -> HandlerResult {
    let call = &result.call;
    if !call.limiter.counted() || call.state != CallModelState::Active {
        return result;
    }
    let due_by = now_ms.saturating_add(::call::millis_i64(period));
    let refresh = TimerKey::call(TimerType::LimiterRefresh);
    let fire_at = match call.timers.iter().find(|t| t.key == refresh).map(|t| t.fire_at) {
        Some(at) if at > now_ms && at <= due_by => return result,
        Some(at) if at <= now_ms => now_ms,
        _ => due_by,
    };
    crate::timer_arm::arm(&mut result.call, &mut result.effects, refresh, fire_at);
    result
}

/// Kind `"limiter"` — a call that sent an admit request releases its key
/// exactly once at termination (the strong admit↔release invariant), with one
/// `release(key)` under the call's own key
/// (`CallLimiterState::owed_release`), which the server applies idempotently
/// and as a no-op for a key it holds nothing for. A call that sent none (no
/// limiter stated, none configured) owes nothing. A `ReleaseLimiter` of the
/// call's key a rule already emitted discharges it; one of another key does
/// not.
pub struct LimiterObligations;

impl ObligationKind for LimiterObligations {
    fn id(&self) -> &'static str {
        "limiter"
    }

    fn settle(&self, call: &Call, effects: &mut HandlerEffects) {
        let Some(owed) = call.limiter.owed_release() else {
            return;
        };
        let already = effects
            .soft
            .iter()
            .any(|e| matches!(e, SoftBoundedEffect::ReleaseLimiter { key } if key == owed));
        if !already {
            effects.soft.push(SoftBoundedEffect::ReleaseLimiter { key: owed.to_string() });
        }
    }

    fn owed(&self, call: &Call) -> Vec<Obligation> {
        call.limiter
            .owed_release()
            .map(|key| Obligation { kind: "limiter", key: key.to_string() })
            .into_iter()
            .collect()
    }
}
