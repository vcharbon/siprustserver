// ── the limiter refresh is always armed on a counted live call ───────────────
use super::fixtures::*;
use super::*;
use b2bua_core::effects::{CriticalStateEffect, HandlerResult};
use call::{CallLimiterState, TimerEntry, TimerKey, TimerType};

/// The turn's refresh arming under a `period_ms` refresh period.
fn arm(result: HandlerResult, now_ms: i64, period_ms: u64) -> HandlerResult {
    b2bua_core::limiter::call::arm_refresh(
        result,
        now_ms,
        std::time::Duration::from_millis(period_ms),
    )
}

fn refresh_timer(fire_at: i64) -> TimerEntry {
    TimerEntry { key: TimerKey::call(TimerType::LimiterRefresh), generation: 1, fire_at }
}

fn scheduled(result: &HandlerResult) -> Vec<i64> {
    result
        .effects
        .critical
        .iter()
        .filter_map(|e| match e {
            CriticalStateEffect::ScheduleTimer(t)
                if *t.timer_type() == TimerType::LimiterRefresh =>
            {
                Some(t.fire_at)
            }
            _ => None,
        })
        .collect()
}

fn counted_active() -> call::Call {
    let mut call = test_call();
    call.state = CallModelState::Active;
    call.limiter = CallLimiterState::admitted(
        call.limiter.key().to_string(),
        1,
        vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
    );
    call
}

#[test]
fn a_dropped_fire_is_re_armed_on_the_next_turn() {
    // The ledger still carries an entry past due — a fire the per-call
    // queue dropped, or a copy's restored one its first turn supersedes:
    // the turn re-arms it due now, on the record too, so the refresh it
    // owes runs at once.
    let mut call = counted_active();
    call.timers = vec![refresh_timer(9_000)];
    let result = arm(HandlerResult::new(call), 10_000, 40_000);
    assert_eq!(scheduled(&result), vec![10_000]);
    let armed: Vec<i64> = result
        .call
        .timers
        .iter()
        .filter(|t| *t.timer_type() == TimerType::LimiterRefresh)
        .map(|t| t.fire_at)
        .collect();
    assert_eq!(armed, vec![10_000], "one entry, replaced");

    // No entry at all: armed as well.
    let result = arm(HandlerResult::new(counted_active()), 10_000, 40_000);
    assert_eq!(scheduled(&result), vec![50_000]);
}

#[test]
fn a_live_refresh_is_left_alone() {
    let mut call = counted_active();
    call.timers = vec![refresh_timer(30_000)];
    let result = arm(HandlerResult::new(call), 10_000, 40_000);
    assert!(scheduled(&result).is_empty(), "{:?}", result.effects.critical);
}

#[test]
fn a_refresh_armed_past_one_period_is_brought_forward() {
    // Armed 40 s ahead, the learnt lease now sets a 2 s period: brought
    // to now + 2 s.
    let mut call = counted_active();
    call.timers = vec![refresh_timer(50_000)];
    let result = arm(HandlerResult::new(call), 10_000, 2_000);
    assert_eq!(scheduled(&result), vec![12_000]);
    let mut call = counted_active();
    call.state = CallModelState::Terminating;
    call.timers = vec![refresh_timer(50_000)];
    let result = arm(HandlerResult::new(call), 10_000, 2_000);
    assert!(scheduled(&result).is_empty(), "an ending call is left alone");
}

#[test]
fn an_uncounted_or_ending_call_arms_nothing() {
    let mut call = counted_active();
    call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
    let result = arm(HandlerResult::new(call), 10_000, 40_000);
    assert!(scheduled(&result).is_empty());
    let mut call = counted_active();
    call.state = CallModelState::Terminating;
    let result = arm(HandlerResult::new(call), 10_000, 40_000);
    assert!(scheduled(&result).is_empty());
}
