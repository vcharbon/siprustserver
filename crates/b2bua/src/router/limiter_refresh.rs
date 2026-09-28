//! The call's side of its limiter refresh: the `LimiterRefresh` turn marks
//! the call due on the worker's refresh batch and re-arms the timer, never
//! waiting on the limiter; the batch's answer comes back as a re-entrant
//! event and is applied on the call's own turn (ADR-0038).

use call::{Call, CallModelState, TimerEntry, TimerType};

use super::RouterCtx;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::limiter::RefreshOutcome;
use crate::limiter_refresh_batch::{outcome_label, RefreshAnswered};

/// A `LimiterRefresh` fire: a counted call marks its key and ids due on the
/// refresh batch under its limiter generation, and re-arms the timer while
/// Active. A Terminating call marks its last refresh and re-arms nothing: its
/// teardown is bounded by the 32 s backstop, inside the lease.
pub(super) fn on_refresh_due(
    ctx: &RouterCtx,
    mut call: Call,
    call_ref: &str,
    now_ms: i64,
) -> HandlerResult {
    let mut fx = HandlerEffects::new();
    if !call.limiter.counted {
        return HandlerResult { call, effects: fx };
    }
    let limiter = &call.limiter;
    ctx.limiter_refreshes.mark(&limiter.key, call_ref, &limiter.ids, limiter.generation);
    if crate::trace::sampled(&call) {
        crate::trace::emit::limiter(&call, now_ms, "refresh", "due");
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

/// Apply `answer` to its resident `call`: `Some` with the call's new state
/// when the answer changes it, `None` when it changes nothing. An answer for
/// another key (an earlier call under the same `call_ref`), for a call no
/// longer counted, or marked under an older limiter generation (a route fold
/// restated the set since) is discarded as stale. `Dropped` (an admit of the
/// key dropped the set) leaves the call uncounted, still owing its release,
/// so it refreshes no more. `Released` and `Reregistered` leave the call as
/// it is: a release fence refuses one lease, then the refresh re-registers.
pub(super) fn apply_answer(
    ctx: &RouterCtx,
    mut call: Call,
    answer: &RefreshAnswered,
    now_ms: i64,
) -> Option<HandlerResult> {
    let limiter = &call.limiter;
    if answer.key != limiter.key || !limiter.counted || answer.generation != limiter.generation {
        ctx.metrics.bump_limiter_refresh_answer_discarded_stale();
        return None;
    }
    ctx.metrics.record_limiter_refresh_answer_applied(outcome_label(answer.outcome));
    if crate::trace::sampled(&call) {
        crate::trace::emit::limiter(&call, now_ms, "refresh", &format!("{:?}", answer.outcome));
    }
    if answer.outcome != RefreshOutcome::Dropped {
        return None;
    }
    call.limiter.set(false, false, Vec::new());
    Some(HandlerResult { call, effects: HandlerEffects::new() })
}

#[cfg(test)]
mod tests {
    use call::CallLimiterState;

    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::router::test_support::{invite, node, src};

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
        call.limiter = CallLimiterState::admitted("c#k".into(), vec!["x".into(), "y".into()]);
        call
    }

    fn answer(outcome: RefreshOutcome, generation: u32) -> RefreshAnswered {
        RefreshAnswered { call_ref: "c".into(), key: "c#k".into(), generation, outcome }
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_answer_leaves_the_call_uncounted_owing_its_release() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let result = apply_answer(ctx, counted_call(), &answer(RefreshOutcome::Dropped, 0), 0)
            .expect("the call changes");
        assert!(!result.call.limiter.counted, "refreshes no more");
        assert!(result.call.limiter.release_owed, "still releases its key");
        assert_eq!(ctx.metrics.limiter_refresh_answers_applied_total("dropped"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_released_or_reregistered_answer_changes_nothing() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        for outcome in [RefreshOutcome::Released, RefreshOutcome::Reregistered] {
            assert!(apply_answer(ctx, counted_call(), &answer(outcome, 0), 0).is_none());
        }
        assert_eq!(ctx.metrics.limiter_refresh_answers_applied_total("released"), 1);
        assert_eq!(ctx.metrics.limiter_refresh_answers_applied_total("reregistered"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_marked_before_a_fold_restated_the_set_is_stale() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let mut call = counted_call();
        call.limiter.set(true, true, vec!["y".into(), "z".into()]);
        assert!(apply_answer(ctx, call, &answer(RefreshOutcome::Dropped, 0), 0).is_none());
        let mut other_key = answer(RefreshOutcome::Dropped, 0);
        other_key.key = "c#earlier".into();
        assert!(apply_answer(ctx, counted_call(), &other_key, 0).is_none());
        assert_eq!(ctx.metrics.limiter_refresh_answers_discarded_stale_total(), 2);
        assert_eq!(ctx.metrics.limiter_refresh_answers_applied_total("dropped"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_reaching_no_resident_call_is_discarded_and_leaves_nothing_behind() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        ctx.reentry_tx.send(answer(RefreshOutcome::Dropped, 0).into_event()).unwrap();
        sip_clock::testkit::settle().await;
        assert_eq!(ctx.metrics.limiter_refresh_answers_discarded_call_gone_total(), 1);
        assert_eq!(n.core.active_calls(), 0, "no call materialised");
        assert_eq!(n.core.lock_count(), 0, "no per-call lock left");
        assert_eq!(ctx.dispatcher.queue_count(), 0, "no per-call queue left");
    }

    #[tokio::test(start_paused = true)]
    async fn a_counted_call_marks_its_refresh_and_re_arms_without_waiting() {
        let n = node("w0").await;
        let ctx = n.core.router_ctx();
        let result = on_refresh_due(ctx, counted_call(), "c", 1_000);
        assert_eq!(ctx.limiter_refreshes.due(), 1, "marked due on the batch");
        let refresh = ctx.config.limiter_refresh_sec * 1000;
        assert!(result
            .call
            .timers
            .iter()
            .any(|t| t.timer_type == TimerType::LimiterRefresh && t.fire_at == 1_000 + refresh));
        let mut uncounted = counted_call();
        uncounted.limiter = CallLimiterState::uncounted("c#k".into());
        on_refresh_due(ctx, uncounted, "c", 1_000);
        assert_eq!(ctx.limiter_refreshes.due(), 1, "an uncounted call marks nothing");
    }
}
