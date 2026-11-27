//! A rule's timer delay at the top of `i64`: the deadline it arms lies in the
//! future, never wrapped behind the clock, and the delay past the supported
//! maximum is a defect noted on the turn.

use super::fixtures::*;
use super::*;

/// A clock far from zero, so a deadline wrapped past `i64::MAX` lands behind it.
const NOW_MS: i64 = 1_700_000_000_000;

fn global_duration() -> TimerKey {
    TimerKey::call(TimerType::GlobalDuration)
}

/// Run `actions` on a fresh call at [`NOW_MS`]: what the executor returns
/// and the defects the turn noted.
fn run(actions: &[RuleAction]) -> (HandlerResult, Vec<&'static str>) {
    let config = B2buaConfig::default();
    let call = test_call();
    let event = info_event();
    let ctx = context(&call, &event, "a", &config).at(NOW_MS);
    let mut id_gen = scope(0x5f);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = NOW_MS;
    let result = exec.execute(actions, &call, &ctx);
    let defects = id_gen.notes.defects().iter().map(|d| d.name).collect();
    (result, defects)
}

/// The deadline `result` holds for [`global_duration`], on the ledger and in
/// its replicated write, which carry the same instant.
fn armed_deadline(result: &HandlerResult) -> i64 {
    let armed =
        result.call.timers.iter().find(|t| t.key == global_duration()).expect("the timer is armed");
    let replicated = result
        .effects
        .critical
        .iter()
        .find_map(|e| match e {
            CriticalStateEffect::ScheduleTimer(t) if t.key == global_duration() => Some(t),
            _ => None,
        })
        .expect("the ledger write is replicated");
    assert_eq!(replicated.fire_at, armed.fire_at, "the replica hydrates the ledger's deadline");
    armed.fire_at
}

/// Arming a timer `i64::MAX` ms out arms it in the future, at the supported
/// maximum, and notes the clamp on the turn.
#[test]
fn a_rule_arming_the_largest_delay_arms_a_deadline_in_the_future() {
    let arm = RuleAction::arm(global_duration(), TimerDelay::millis(i64::MAX));
    let (result, defects) = run(&[arm]);
    let fire_at = armed_deadline(&result);
    assert!(fire_at > NOW_MS, "the deadline {fire_at} lies after the clock {NOW_MS}");
    assert_eq!(defects.len(), 1, "the clamped delay is one defect on the turn: {defects:?}");
}

/// Snoozing a timer `i64::MAX` ms out re-arms it as arming does: in the future,
/// at the supported maximum, the clamp noted on the turn.
#[test]
fn a_rule_snoozing_by_the_largest_delay_re_arms_a_deadline_in_the_future() {
    let snooze = RuleAction::snooze(global_duration(), TimerDelay::millis(i64::MAX));
    let (result, defects) = run(&[snooze]);
    let fire_at = armed_deadline(&result);
    assert!(fire_at > NOW_MS, "the deadline {fire_at} lies after the clock {NOW_MS}");
    assert_eq!(defects.len(), 1, "the clamped delay is one defect on the turn: {defects:?}");
}
