//! `RuleAction::SnoozeTimer` (CONTEXT.md "Snooze"): the fired timer armed again
//! one step out, each snooze counted until the key is armed fresh.

use super::fixtures::*;
use super::*;

const NOW_MS: i64 = 1_000_000;

fn global_duration() -> TimerKey {
    TimerKey::call(TimerType::GlobalDuration)
}

/// Run `actions` on `call` at [`NOW_MS`]; the call they leave and the
/// defects the turn noted.
fn run(call: &call::Call, actions: &[RuleAction]) -> (call::Call, Vec<&'static str>) {
    let config = B2buaConfig::default();
    let event = info_event();
    let ctx = context(call, &event, "a", &config).at(NOW_MS);
    let mut id_gen = scope(0x5e);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = NOW_MS;
    let result = exec.execute(actions, call, &ctx);
    let defects = id_gen.notes.defects().iter().map(|d| d.name).collect();
    (result.call, defects)
}

/// Each snooze re-arms the key `delay` out and counts one more; the rule
/// reads the count through its view of the call.
#[test]
fn a_snooze_rearms_the_timer_and_counts() {
    let snooze = RuleAction::snooze(global_duration(), TimerDelay::secs(1));
    let (once, defects) = run(&test_call(), std::slice::from_ref(&snooze));
    assert!(defects.is_empty(), "{defects:?}");
    let armed = once.timers.iter().find(|t| t.key == global_duration()).expect("re-armed");
    assert_eq!(armed.fire_at, NOW_MS + 1_000, "the timer fires again one step out");
    assert_eq!(b2bua_sdk::rules::RuleCall::new(&once).snoozes(&global_duration()), 1);

    let (twice, _) = run(&once, &[snooze]);
    assert_eq!(b2bua_sdk::rules::RuleCall::new(&twice).snoozes(&global_duration()), 2);
}

/// Arming the key fresh drops its count: a later deadline starts unsnoozed.
#[test]
fn arming_the_key_fresh_drops_its_snoozes() {
    let snooze = RuleAction::snooze(global_duration(), TimerDelay::secs(1));
    let (snoozed, _) = run(&test_call(), &[snooze]);
    let (rearmed, _) = run(&snoozed, &[RuleAction::arm(global_duration(), TimerDelay::secs(3600))]);
    assert_eq!(b2bua_sdk::rules::RuleCall::new(&rearmed).snoozes(&global_duration()), 0);
    assert!(rearmed.snoozes.is_empty(), "no entry left: {:?}", rearmed.snoozes);
}

/// A framework timer is never a rule's to snooze, as it is never a rule's to
/// arm.
#[test]
fn a_rule_snoozing_a_framework_timer_is_refused() {
    let key = TimerKey::call(TimerType::HeldAnswer);
    let (call, defects) =
        run(&test_call(), &[RuleAction::snooze(key.clone(), TimerDelay::secs(1))]);
    assert!(call.timers.iter().all(|t| t.key != key), "not armed");
    assert!(call.snoozes.is_empty(), "not counted");
    assert_eq!(defects, [b2bua_core::turn_notes::FRAMEWORK_TIMER]);
}

/// A rule sees a release event's consult as awaited from the moment it is sent
/// until its answer takes it off the call.
#[test]
fn a_release_consult_is_awaited_until_answered() {
    let mut call = test_call();
    assert!(!b2bua_sdk::rules::RuleCall::new(&call).release_awaited());
    call.awaited.push(call::OffTurnRequest::Release {
        request: serde_json::json!({ "event": "max_call_duration" }),
        limiter: call::ConsultAdmits { first_change: 1, may_hold: true },
    });
    assert!(b2bua_sdk::rules::RuleCall::new(&call).release_awaited());
}
