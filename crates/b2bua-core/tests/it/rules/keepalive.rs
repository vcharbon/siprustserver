//! The keepalive ledger ceiling, on its arming side.

use super::fixtures::*;
use super::*;

/// A `RuleContext` for `event` on `call` at `now_ms`, acting on the a-leg.
fn timer_ctx<'a>(
    call: &'a call::Call,
    event: &'a CallEvent,
    config: &'a B2buaConfig,
    now_ms: i64,
) -> RuleContext<'a> {
    context(call, event, "a", config).at(now_ms)
}

/// The `keepalive` rule's own re-arm enters the ledger at exactly the configured
/// cadence — never farther — so no restored call can inherit a deadline the
/// restore seam has to clamp, and a policy deadline that legitimately outlives a
/// probe cadence (`GlobalDuration`) is untouched.
#[test]
fn the_keepalive_rule_arms_its_ledger_deadline_at_exactly_one_cadence() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let interval_ms = config.keepalive_interval_sec * 1000;
    let call = test_call();
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::Keepalive),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let keepalive = ranked.iter().find(|r| r.id == "keepalive").expect("keepalive is a candidate");
    let mut actions = (keepalive.handle)(&ctx).expect("keepalive handles its own timer").actions;
    actions
        .push(RuleAction::arm(TimerKey::call(TimerType::GlobalDuration), TimerDelay::secs(3600)));

    let mut id_gen = scope(0x65);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = now_ms;
    let result = exec.execute(&actions, &call, &ctx);

    let armed = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::Keepalive)
        .expect("the rule re-arms the probe");
    assert_eq!(
        armed.fire_at,
        now_ms + interval_ms,
        "the probe is re-armed one cadence out, in this node's clock frame",
    );
    let duration = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::GlobalDuration)
        .expect("the policy timer is armed");
    assert_eq!(
        duration.fire_at,
        now_ms + 3_600_000,
        "a non-keepalive deadline keeps its full delay — the ceiling is keepalive-only",
    );
    let replicated = result
        .effects
        .critical
        .iter()
        .find_map(|e| match e {
            CriticalStateEffect::ScheduleTimer(t) if *t.timer_type() == TimerType::Keepalive => {
                Some(t)
            }
            _ => None,
        })
        .expect("the probe's ledger write is replicated");
    assert_eq!(
        replicated.fire_at, armed.fire_at,
        "the replicated copy a peer later hydrates carries the ledger's capped deadline",
    );
}

/// An arming site that computes a `Keepalive` deadline beyond one cadence is a
/// defect at that site, noted on the turn, and the deadline is clamped to one
/// interval out: an early probe costs nothing, a probe a cadence late costs
/// the call its peer's keepalive tolerance, and a foreign-frame deadline is
/// never persisted and replicated.
#[test]
fn arming_a_keepalive_beyond_one_cadence_is_a_defect_and_is_clamped() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let call = test_call();
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::Keepalive),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let mut id_gen = scope(0x65);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = now_ms;
    let result = exec.execute(
        &[RuleAction::arm(
            TimerKey::call(TimerType::Keepalive),
            TimerDelay::secs(config.keepalive_interval_sec * 2),
        )],
        &call,
        &ctx,
    );
    let armed = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::Keepalive)
        .expect("the probe is armed");
    assert_eq!(
        armed.fire_at,
        now_ms + config.keepalive_interval_sec * 1000,
        "the over-cadence deadline is clamped to one interval out",
    );
    let defects: Vec<_> = id_gen.notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [b2bua_core::turn_notes::KEEPALIVE_WITHIN_CADENCE]);
}

/// A service re-arming the `Keepalive` at 0 ms, as a defect resetting its
/// message window on every turn would, gets it armed one full interval out:
/// the window resets at most once per interval (ADR-0057 §1). The arm is no
/// defect: asking for less than the cadence is not a foreign deadline.
#[test]
fn a_keepalive_re_armed_at_zero_fires_one_interval_out() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let call = test_call();
    let event = info_event();
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let mut id_gen = scope(0x65);
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = now_ms;
    let arm = RuleAction::arm(TimerKey::call(TimerType::Keepalive), TimerDelay::millis(0));
    let result = exec.execute(&[arm], &call, &ctx);
    let armed = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::Keepalive)
        .expect("the probe is armed");
    assert_eq!(armed.fire_at, now_ms + config.keepalive_interval_sec * 1000);
    assert!(id_gen.notes.defects().is_empty(), "{:?}", id_gen.notes.defects());
}

/// `Rung`, `NotifyAnswerCap` and `HeldAnswer` are the framework's: neither
/// message cap counts their fires, so a rule's arm of one is dropped as a
/// defect (ADR-0057 §1).
#[test]
fn a_rule_arming_a_framework_timer_is_refused() {
    let config = B2buaConfig::default();
    let call = test_call();
    let event = info_event();
    let ctx = timer_ctx(&call, &event, &config, 1_000);
    let obligation =
        call::Obligation::AckOf2xx { leg: "a".into(), dialog_tag: "t".into(), cseq: 1 };
    for key in [
        TimerKey::obligation(TimerType::Rung, obligation),
        TimerKey::call(TimerType::NotifyAnswerCap),
        TimerKey::call(TimerType::HeldAnswer),
    ] {
        let mut id_gen = scope(0x65);
        let mut exec = executor(&config, &mut id_gen);
        let result =
            exec.execute(&[RuleAction::arm(key.clone(), TimerDelay::secs(1))], &call, &ctx);
        assert!(result.call.timers.iter().all(|t| t.key != key), "{key:?} is not armed");
        assert!(result.effects.critical.is_empty(), "{key:?}: nothing reaches the driver");
        let defects: Vec<_> = id_gen.notes.defects().iter().map(|d| d.name).collect();
        assert_eq!(defects, [b2bua_core::turn_notes::FRAMEWORK_TIMER], "{key:?}");
    }
}
