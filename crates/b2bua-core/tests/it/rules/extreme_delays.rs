//! A rule's timer delay at the top of `i64`: the deadline it arms lies in the
//! future, never wrapped behind the clock, and the delay past the supported
//! maximum is a defect noted on the turn.

use b2bua_core::turn_notes::{DELAY_WITHIN_MAXIMUM, KEEPALIVE_WITHIN_CADENCE};
use b2bua_sdk::timer_delay::MAX_DELAY_MS;
use call::features::FeatureActivations;

use super::fixtures::*;
use super::*;

/// A clock far from zero, so a deadline wrapped past `i64::MAX` lands behind it.
const NOW_MS: i64 = 1_700_000_000_000;

fn global_duration() -> TimerKey {
    TimerKey::call(TimerType::GlobalDuration)
}

/// Run `actions` on a fresh call at [`NOW_MS`] under `config`: what the
/// executor returns and the defects the turn noted.
fn run_under(config: &B2buaConfig, actions: &[RuleAction]) -> (HandlerResult, Vec<&'static str>) {
    let call = test_call();
    let event = info_event();
    let ctx = context(&call, &event, "a", config).at(NOW_MS);
    let mut id_gen = scope(0x5f);
    let mut exec = executor(config, &mut id_gen);
    exec.now_ms = NOW_MS;
    let result = exec.execute(actions, &call, &ctx);
    let defects = id_gen.notes.defects().iter().map(|d| d.name).collect();
    (result, defects)
}

/// [`run_under`] the default configuration.
fn run(actions: &[RuleAction]) -> (HandlerResult, Vec<&'static str>) {
    run_under(&B2buaConfig::default(), actions)
}

/// The deadline `result` holds for `key`, on the ledger and in its
/// replicated write, which carry the same instant.
fn armed_deadline(result: &HandlerResult, key: &TimerKey) -> i64 {
    let armed = result.call.timers.iter().find(|t| &t.key == key).expect("the timer is armed");
    let replicated = result
        .effects
        .critical
        .iter()
        .find_map(|e| match e {
            CriticalStateEffect::ScheduleTimer(t) if &t.key == key => Some(t),
            _ => None,
        })
        .expect("the ledger write is replicated");
    assert_eq!(replicated.fire_at, armed.fire_at, "the replica hydrates the ledger's deadline");
    armed.fire_at
}

/// Arming a timer `i64::MAX` ms out arms it at the supported maximum, about
/// 24.8 days out, and notes the clamp on the turn.
#[test]
fn a_rule_arming_the_largest_delay_arms_a_deadline_in_the_future() {
    let arm = RuleAction::arm(global_duration(), TimerDelay::millis(i64::MAX));
    let (result, defects) = run(&[arm]);
    assert_eq!(armed_deadline(&result, &global_duration()), NOW_MS + 2_147_483_647);
    assert_eq!(defects, [DELAY_WITHIN_MAXIMUM], "the clamped delay is one defect on the turn");
}

/// Snoozing a timer `i64::MAX` ms out re-arms it as arming does: at the
/// supported maximum, the clamp noted on the turn.
#[test]
fn a_rule_snoozing_by_the_largest_delay_re_arms_a_deadline_in_the_future() {
    let snooze = RuleAction::snooze(global_duration(), TimerDelay::millis(i64::MAX));
    let (result, defects) = run(&[snooze]);
    assert_eq!(armed_deadline(&result, &global_duration()), NOW_MS + 2_147_483_647);
    assert_eq!(defects, [DELAY_WITHIN_MAXIMUM], "the clamped delay is one defect on the turn");
}

/// A delay of exactly the supported maximum is armed as stated, no defect.
#[test]
fn a_rule_arming_the_maximum_delay_arms_it_as_stated() {
    let arm = RuleAction::arm(global_duration(), TimerDelay::millis(MAX_DELAY_MS));
    let (result, defects) = run(&[arm]);
    assert_eq!(armed_deadline(&result, &global_duration()), NOW_MS + 2_147_483_647);
    assert!(defects.is_empty(), "a delay at the maximum is no defect: {defects:?}");
}

/// A `Keepalive` armed `i64::MAX` seconds out is held to one cadence, the
/// cadence's own defect noted once.
#[test]
fn a_keepalive_armed_the_largest_delay_takes_one_cadence() {
    let key = TimerKey::call(TimerType::Keepalive);
    let config = B2buaConfig::default();
    let cadence_ms = config.keepalive_interval_sec * 1000;
    let (result, defects) = run(&[RuleAction::arm(key.clone(), TimerDelay::secs(i64::MAX))]);
    assert_eq!(armed_deadline(&result, &key), NOW_MS + cadence_ms);
    assert_eq!(defects, [KEEPALIVE_WITHIN_CADENCE], "one defect, the cadence's");
}

/// With probing off, a `Keepalive` armed `i64::MAX` ms out is held to the
/// supported maximum as any other timer is.
#[test]
fn a_keepalive_with_probing_off_is_held_to_the_maximum() {
    let key = TimerKey::call(TimerType::Keepalive);
    let config = B2buaConfig { keepalive_interval_sec: 0, ..B2buaConfig::default() };
    let (result, defects) =
        run_under(&config, &[RuleAction::arm(key.clone(), TimerDelay::millis(i64::MAX))]);
    assert_eq!(armed_deadline(&result, &key), NOW_MS + 2_147_483_647);
    assert_eq!(defects, [DELAY_WITHIN_MAXIMUM]);
}

/// A rule stating the call's features with a maximum duration of `i64::MAX`
/// ms leaves the call granted the supported maximum.
#[test]
fn stated_features_hold_the_maximum_duration_to_the_maximum_delay() {
    let mut features: FeatureActivations = b2bua_core::decision::default_platform_features();
    features.platform.max_duration_ms = i64::MAX;
    let (result, _) = run(&[RuleAction::SetFeatures { features }]);
    let granted = result.call.features.expect("the features are set").platform.max_duration_ms;
    assert_eq!(granted, 2_147_483_647);
}
