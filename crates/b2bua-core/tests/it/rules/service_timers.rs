// ── Service-owned timers (TimerType::Service) ────────────────────────────────
//
// End-to-end fire/cancel/wildcard behaviour lives in
// `b2bua-harness/tests/service_timers.rs`; here we pin the *identity* seams at
// the executor level: distinct keys are distinct ledger entries, a same-key
// re-schedule supersedes (replace, not append), the recipe-minted cancel
// removes exactly its own entry, and matcher scoping keeps core and foreign
// services out of a service's fires.
use super::fixtures::*;
use super::*;

const SVC: MachineId = MachineId::new("svc-a");

fn schedule(t: TimerType, secs: i64) -> RuleAction {
    RuleAction::arm(TimerKey::call(t), TimerDelay::secs(secs))
}

#[test]
fn distinct_keys_coexist_and_same_key_reschedule_supersedes() {
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let call = test_call();
    let mut exec = executor(&config, &mut id_gen);
    exec.now_ms = 1_000;
    let event = info_like_event(&call);
    let ctx = ctx_at(&call, &event, &config);

    let fast = TimerType::service(SVC, "fast");
    let slow = TimerType::service(SVC, "slow");
    let result = exec.execute(&[schedule(fast.clone(), 3), schedule(slow.clone(), 6)], &call, &ctx);
    let ids: Vec<String> = result.call.timers.iter().map(|t| format!("{:?}", t.key)).collect();
    assert_eq!(
        ids,
        vec!["Service:svc-a:fast on call", "Service:svc-a:slow on call"],
        "two keys → two distinct ledger entries",
    );

    // Re-arm the SAME key: replaced in place, not appended.
    let ctx2 = ctx_at(&result.call, &event, &config);
    let rearmed = exec.execute(&[schedule(fast.clone(), 10)], &result.call, &ctx2);
    assert_eq!(rearmed.call.timers.len(), 2, "same-key re-arm supersedes");
    let entry =
        rearmed.call.timers.iter().find(|t| *t.timer_type() == fast).expect("fast entry present");
    assert_eq!(entry.fire_at, 1_000 + 10_000, "the re-arm's deadline won");
    assert_eq!(*entry.timer_type(), fast);

    // A cancel of the key removes exactly its own entry.
    let ctx3 = ctx_at(&rearmed.call, &event, &config);
    let cancelled = exec.execute(&[RuleAction::cancel(TimerKey::call(fast))], &rearmed.call, &ctx3);
    let ids: Vec<String> = cancelled.call.timers.iter().map(|t| format!("{:?}", t.key)).collect();
    assert_eq!(ids, vec!["Service:svc-a:slow on call"], "cancel hits only its own key");
}

/// Matcher scoping: a core timer rule never sees a service fire; a
/// service's exact-key / wildcard matchers never see core fires or another
/// service's fires.
#[test]
fn service_fires_are_invisible_to_core_and_foreign_services() {
    let config = B2buaConfig::default();
    let call = test_call();

    let mine = TimerType::service(SVC, "fast");
    let service_fire = CallEvent::Timer {
        key: TimerKey::call(mine.clone()),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let core_fire = CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let foreign_fire = CallEvent::Timer {
        key: TimerKey::call(TimerType::service(MachineId::new("svc-b"), "fast")),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };

    // No CORE rule matches a service fire (core rules pin unit variants).
    let ctx = ctx_at(&call, &service_fire, &config);
    assert!(
        pick_ranked(&default_rules(), &call, &ctx).is_empty(),
        "core stays ignorant of service timers",
    );

    let exact = Match::timer().timer_type(mine.clone());
    let wildcard = Match::timer().service_timers(SVC);
    for (name, m) in [("exact", &exact), ("wildcard", &wildcard)] {
        let ctx = ctx_at(&call, &service_fire, &config);
        assert!(m.accepts_columns(&ctx), "{name} matcher accepts its own fire");
        let ctx = ctx_at(&call, &core_fire, &config);
        assert!(!m.accepts_columns(&ctx), "{name} matcher rejects a core fire");
        let ctx = ctx_at(&call, &foreign_fire, &config);
        assert!(!m.accepts_columns(&ctx), "{name} matcher rejects another service's fire");
    }
    // Exact-key is exact: same service, different key → no match.
    let other_key_fire = CallEvent::Timer {
        key: TimerKey::call(TimerType::service(SVC, "slow")),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = ctx_at(&call, &other_key_fire, &config);
    assert!(!exact.accepts_columns(&ctx), "exact-key matcher rejects a sibling key");
    assert!(wildcard.accepts_columns(&ctx), "per-service wildcard accepts a sibling key");
}

fn info_like_event(call: &call::Call) -> CallEvent {
    CallEvent::Timer {
        key: TimerKey::call(TimerType::service(SVC, "fast")),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    }
}

fn ctx_at<'a>(
    call: &'a call::Call,
    event: &'a CallEvent,
    config: &'a B2buaConfig,
) -> RuleContext<'a> {
    context(call, event, "a", config).at(1_000)
}
