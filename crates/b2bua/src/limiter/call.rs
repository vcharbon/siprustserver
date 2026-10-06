//! The call's side of its limiter: the refresh it keeps armed and the
//! release it owes.
//!
//! [`arm_refresh`] is the one arming of a counted call's `LimiterRefresh`,
//! run on every turn. The refresh turn marks the call due on the worker's
//! refresh batch, never waiting on the limiter; the batch's answer comes back
//! as a re-entrant event and is applied on the call's own turn (ADR-0040).
//! `LimiterObligations` is the release a call owes at termination.

use call::{Call, CallModelState, RefreshApplied, TimerEntry, TimerType};

use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult, SoftBoundedEffect};
use crate::limiter::refresh_batch::RefreshAnswered;
use crate::limiter::LimiterWorker;
use crate::metrics::{B2buaMetrics, RefreshDiscard};
use crate::obligations::{Obligation, ObligationKind};

/// A counted Active call carries a live `LimiterRefresh` timer due within one
/// refresh `period` of the learnt lease. A missing or past-due entry (the
/// turn's own admit, the refresh turn whose entry just fired, a fire the
/// per-call queue dropped, a copy materialised with a stale ledger) and one
/// due later than `now_ms + period` (armed under a longer period) are
/// re-armed at `now_ms + period`, on the record and as a `ScheduleTimer`.
pub fn arm_refresh(
    mut result: HandlerResult,
    now_ms: i64,
    period: std::time::Duration,
) -> HandlerResult {
    let call = &result.call;
    if !call.limiter.counted() || call.state != CallModelState::Active {
        return result;
    }
    let due_by = now_ms + period.as_millis() as i64;
    let live = call.timers.iter().any(|t| {
        t.timer_type == TimerType::LimiterRefresh && t.fire_at > now_ms && t.fire_at <= due_by
    });
    if live {
        return result;
    }
    let entry = TimerEntry {
        id: format!("{:?}", TimerType::LimiterRefresh),
        timer_type: TimerType::LimiterRefresh,
        fire_at: due_by,
        leg_id: None,
    };
    result.call.timers =
        call::helpers::replace_timer_by_id(std::mem::take(&mut result.call.timers), entry.clone());
    result.effects.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    result
}

/// A `LimiterRefresh` fire: a counted call marks its key due on `limiter`'s
/// refresh batch with the set it holds. The turn's [`arm_refresh`] re-arms
/// the fired timer while Active; a Terminating call marks its last refresh
/// and is not re-armed: its teardown is bounded by the 32 s backstop, inside
/// the lease.
pub(crate) fn on_refresh_due(
    limiter: &LimiterWorker,
    call: Call,
    call_ref: &str,
    now_ms: i64,
) -> HandlerResult {
    if call.limiter.counted() {
        limiter.refresh_due(call.limiter.key(), call_ref, &call.limiter.held_set());
        if crate::trace::sampled(&call) {
            crate::trace::emit::limiter(&call, now_ms, "refresh", "due");
        }
    }
    HandlerResult::new(call)
}

/// Apply `answer` to its resident `call`: `Some` with the call's new state
/// when the answer changes it, `None` when it changes nothing. An answer for
/// another key (an earlier call under the same `call_ref`), or stating a set
/// older than the one the call holds (an admit restated the set since), is
/// discarded as stale (counted on `metrics`). Otherwise the set it states
/// becomes the call's held set (`CallLimiterState::apply_refresh`): `Dropped`
/// (an admit of the key dropped the set) leaves the call uncounted, still
/// owing its release, so it refreshes no more; an `Extended` stating another
/// set repairs an admit whose answer was lost. `Released` leaves the call as
/// it is: a release fence refuses one lease, then the refresh re-registers.
pub(crate) fn apply_answer(
    metrics: &B2buaMetrics,
    mut call: Call,
    answer: &RefreshAnswered,
    now_ms: i64,
) -> Option<HandlerResult> {
    let applied = call.limiter.apply_refresh(&answer.key, answer.held.as_ref());
    if applied == RefreshApplied::Stale {
        metrics.limiter().count_refresh_discarded(RefreshDiscard::Stale);
        return None;
    }
    if crate::trace::sampled(&call) {
        crate::trace::emit::limiter(&call, now_ms, "refresh", &format!("{:?}", answer.outcome));
    }
    (applied == RefreshApplied::Changed).then(|| HandlerResult::new(call))
}

/// Kind `"limiter"` — a call that sent an admit request releases its key
/// exactly once at termination (the strong admit↔release invariant), with one
/// `release(key)` under the call's own key
/// (`CallLimiterState::owed_release`), which the server applies idempotently
/// and as a no-op for a key it holds nothing for. A call that sent none (no
/// limiter stated, none configured) owes nothing. A `ReleaseLimiter` of the
/// call's key a rule already emitted discharges it; one of another key does
/// not.
pub(crate) struct LimiterObligations;

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

#[cfg(test)]
mod tests {
    use call::{CallLimiterState, LimiterEntry, LimiterHeld};

    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::limiter::RefreshOutcome;
    use crate::router::test_support::{invite, node, src};

    fn entries(ids: &[&str]) -> Vec<LimiterEntry> {
        ids.iter().map(|id| LimiterEntry { id: id.to_string(), limit: 10 }).collect()
    }

    /// A call counted on `[x, y]`, stated under change 2.
    fn counted_call() -> Call {
        let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let mut call = build_initial_call(
            &invite("w0", "w1", "refresh"),
            src(),
            &config,
            &sip_txn::IdGen::seeded(1),
            0,
        );
        call.state = CallModelState::Active;
        call.limiter = CallLimiterState::admitted("c#k".into(), 2, entries(&["x", "y"]));
        call
    }

    fn answer(outcome: RefreshOutcome, held: Option<(u64, &[&str])>) -> RefreshAnswered {
        RefreshAnswered {
            call_ref: "c".into(),
            key: "c#k".into(),
            outcome,
            held: held.map(|(change, ids)| LimiterHeld { change, entries: entries(ids) }),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_answer_leaves_the_call_uncounted_owing_its_release() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let dropped = answer(RefreshOutcome::Dropped, Some((2, &[])));
        let result =
            apply_answer(&ctx.metrics, counted_call(), &dropped, 0).expect("the call changes");
        assert!(!result.call.limiter.counted(), "refreshes no more");
        assert!(result.call.limiter.owed_release().is_some(), "still releases its key");
        assert!(result.call.limiter.fail_open(), "runs uncounted on a target naming ids");
    }

    #[tokio::test(start_paused = true)]
    async fn a_released_or_reregistered_answer_changes_nothing() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let released = answer(RefreshOutcome::Released, None);
        assert!(apply_answer(&ctx.metrics, counted_call(), &released, 0).is_none());
        let reregistered = answer(RefreshOutcome::Reregistered, Some((2, &["x", "y"])));
        assert!(apply_answer(&ctx.metrics, counted_call(), &reregistered, 0).is_none());
        assert_eq!(ctx.metrics.limiter().refresh_discarded_total(RefreshDiscard::Stale), 0);
    }

    /// An admit landed and its answer was lost: the refresh answer states the
    /// newer set, which becomes the call's held set.
    #[tokio::test(start_paused = true)]
    async fn an_answer_stating_a_newer_set_repairs_a_lost_admit_answer() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let newer = answer(RefreshOutcome::Extended, Some((3, &["y", "z"])));
        let result =
            apply_answer(&ctx.metrics, counted_call(), &newer, 0).expect("the call changes");
        let mut limiter = result.call.limiter;
        assert_eq!(limiter.held(), entries(&["y", "z"]));
        assert_eq!(limiter.held_set().change, 3);
        assert_eq!(limiter.number_admit().0, 4, "the next admit is numbered above it");
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_older_than_the_held_set_or_for_another_key_is_stale() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let older = answer(RefreshOutcome::Dropped, Some((1, &[])));
        assert!(apply_answer(&ctx.metrics, counted_call(), &older, 0).is_none());
        let mut other_key = answer(RefreshOutcome::Dropped, Some((9, &[])));
        other_key.key = "c#earlier".into();
        assert!(apply_answer(&ctx.metrics, counted_call(), &other_key, 0).is_none());
        assert_eq!(ctx.metrics.limiter().refresh_discarded_total(RefreshDiscard::Stale), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_reaching_no_resident_call_is_discarded_and_leaves_nothing_behind() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        ctx.reentry_tx.send(answer(RefreshOutcome::Dropped, Some((2, &[]))).into_event()).unwrap();
        sip_clock::testkit::settle().await;
        assert_eq!(ctx.metrics.limiter().refresh_discarded_total(RefreshDiscard::CallGone), 1);
        assert_eq!(n.core.active_calls(), 0, "no call materialised");
        assert_eq!(n.core.lock_count(), 0, "no per-call lock left");
        assert_eq!(ctx.dispatcher.queue_count(), 0, "no per-call queue left");
    }

    /// The refresh turn as the router runs it: the fire, then the turn's
    /// refresh arming.
    #[tokio::test(start_paused = true)]
    async fn a_counted_call_marks_its_refresh_and_re_arms_without_waiting() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let period = ctx.limiter.refresh_period();
        let mut fired = counted_call();
        fired.timers = vec![TimerEntry {
            id: format!("{:?}", TimerType::LimiterRefresh),
            timer_type: TimerType::LimiterRefresh,
            fire_at: 1_000,
            leg_id: None,
        }];
        let result = arm_refresh(on_refresh_due(&ctx.limiter, fired, "c", 1_000), 1_000, period);
        assert_eq!(ctx.limiter.refreshes_due(), 1, "marked due on the batch");
        let refresh = period.as_millis() as i64;
        assert!(result
            .call
            .timers
            .iter()
            .any(|t| t.timer_type == TimerType::LimiterRefresh && t.fire_at == 1_000 + refresh));
        let mut uncounted = counted_call();
        uncounted.limiter = CallLimiterState::uncounted("c#k".into());
        on_refresh_due(&ctx.limiter, uncounted, "c", 1_000);
        assert_eq!(ctx.limiter.refreshes_due(), 1, "an uncounted call marks nothing");
    }
}
