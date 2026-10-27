//! The limiter settle: one release per call that sent an admit.

use super::fixtures::*;
use super::*;
use b2bua_core::effects::{FireAndForgetEffect, HandlerEffects, SoftBoundedEffect};
use b2bua_core::obligations::ObligationSet;
use b2bua_core::rule_fixture::executor;
use call::CallLimiterState;

/// The releases the turn sends.
fn releases(effects: &HandlerEffects) -> usize {
    effects.soft.iter().filter(|e| matches!(e, SoftBoundedEffect::ReleaseLimiter { .. })).count()
}

#[test]
fn a_counted_call_is_released_once_whatever_its_set() {
    let mut call = test_call();
    call.limiter = CallLimiterState::admitted(
        call.limiter.key().to_string(),
        1,
        vec![
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "x".into(), limit: 10 },
            call::LimiterEntry { id: "y".into(), limit: 10 },
        ],
    );
    let mut effects = HandlerEffects::new();
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
    assert_eq!(ObligationSet::core().owed(&call).len(), 2, "the limiter and the CDR");

    // Settling again discharges nothing more.
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
}

#[test]
fn a_rule_emitted_release_discharges_the_call() {
    let mut call = test_call();
    call.limiter = CallLimiterState::admitted(
        call.limiter.key().to_string(),
        1,
        vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
    );
    let mut effects = HandlerEffects::new();
    effects.soft.push(SoftBoundedEffect::ReleaseLimiter { key: call.limiter.key().to_string() });
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
}

#[test]
fn a_call_that_sent_no_admit_owes_no_release() {
    let mut call = test_call();
    call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
    let mut effects = HandlerEffects::new();
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 0, "{:?}", effects.soft);
    assert!(ObligationSet::core().owed(&call).iter().all(|o| o.kind == "cdr"));
}

#[test]
fn an_uncounted_call_that_sent_an_admit_is_released_once() {
    let mut call = test_call();
    call.limiter = unconfirmed(call.limiter.key());
    let mut effects = HandlerEffects::new();
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
    assert_eq!(ObligationSet::core().owed(&call).len(), 2, "the limiter and the CDR");
    ObligationSet::core().settle(&call, &mut effects);
    assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
}

/// Execute `actions` on `call` for a turn, then enforce.
fn turn(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let event = fold_event(call, "call-failure-result", "failover", route_fold_payload());
    let config = B2buaConfig::default();
    let ctx = ctx_for(call, &event, &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let result = exec.execute(actions, call, &ctx);
    invariants::enforce(
        &ObligationSet::core(),
        call,
        result,
        0,
        invariants::UnansweredCaller::Leave,
        &mut TurnNotes::default(),
    )
}

#[test]
fn a_call_ended_holding_a_set_or_after_a_dropped_one_releases_once() {
    // One release of the call, which frees whatever set the limiter
    // holds for it; a call that never sent an admit ends with none.
    let end = [RuleAction::TerminateCall { cause: TerminationCause::Supervisor, by_leg: None }];
    let mut call = test_call();
    call.limiter = CallLimiterState::admitted(
        call.limiter.key().to_string(),
        1,
        vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
    );
    let result = turn(&call, &end);
    assert_eq!(result.call.state, CallModelState::Terminated, "the turn ends the call");
    assert_eq!(releases(&result.effects), 1, "{:?}", result.effects.soft);

    call.limiter = unconfirmed(call.limiter.key());
    let result = turn(&call, &end);
    assert_eq!(releases(&result.effects), 1, "{:?}", result.effects.soft);

    call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
    let result = turn(&call, &end);
    assert_eq!(result.call.state, CallModelState::Terminated);
    assert_eq!(releases(&result.effects), 0, "{:?}", result.effects.soft);
}

/// A turn that sends an admit off the call's turn (a service's
/// replacement, a failure consult, a release consult) owes the call's
/// release from that turn on: the admit may land while the call is taken
/// over or ended before its report comes back.
#[test]
fn a_turn_sending_an_admit_owes_the_release() {
    for action in [
        RuleAction::ReplaceAdmissionSet {
            correlation_id: "svc:1".into(),
            entries: vec![call::LimiterEntry { id: "x".into(), limit: 1 }],
            moves_call: false,
        },
        RuleAction::FailureAsyncHttp { request: serde_json::json!({}) },
        RuleAction::ReleaseAsyncHttp { request: serde_json::json!({}) },
    ] {
        let mut call = test_call();
        call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
        let result = turn(&call, std::slice::from_ref(&action));
        assert!(result.call.limiter.owed_release().is_some(), "{action:?} owes the release");
    }
}

/// A service's replacement is the call's target from the turn that sends
/// it: a change computed before its report comes back sees it.
#[test]
fn a_service_replacement_is_the_target_from_its_sending_turn() {
    let mut call = test_call();
    call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
    let entries = vec![call::LimiterEntry { id: "x".into(), limit: 1 }];
    let action = RuleAction::ReplaceAdmissionSet {
        correlation_id: "svc:1".into(),
        entries: entries.clone(),
        moves_call: false,
    };
    let result = turn(&call, &[action]);
    assert_eq!(result.call.limiter.target(), entries);
}

/// A service moves a call that holds nothing and owes nothing onto an
/// empty set: nothing can have landed and nothing is asked, so no admit
/// leaves; the move is recorded on the turn, and the result re-enters as
/// `not_sent`.
#[test]
fn a_move_onto_nothing_with_nothing_to_send_sends_no_admit() {
    let mut call = test_call();
    let moved_onto = vec![call::LimiterEntry { id: "y".into(), limit: 1 }];
    call.limiter = CallLimiterState::from_parts(
        call.limiter.key().to_string(),
        false,
        (Vec::new(), 0),
        (Vec::new(), 0),
        0,
        Some(moved_onto),
    );
    assert!(call.limiter.runs_uncounted(), "the call runs on y, which nothing holds");
    let action = RuleAction::ReplaceAdmissionSet {
        correlation_id: "svc:1".into(),
        entries: Vec::new(),
        moves_call: true,
    };
    let result = turn(&call, &[action]);
    assert_eq!(result.call.limiter.runs_on(), Some(&[][..]), "moved onto nothing");
    assert!(!result.call.limiter.runs_uncounted() && result.call.limiter.owed_release().is_none());
    let ff = &result.effects.fire_and_forget;
    assert!(
        !ff.iter().any(|e| matches!(e, FireAndForgetEffect::LimiterAdmit { .. })),
        "no admit leaves: {ff:?}"
    );
    assert!(
        ff.iter().any(|e| matches!(e, FireAndForgetEffect::Reenter(ev)
            if matches!(ev.as_ref(), CallEvent::InternalEvent { topic, outcome, .. }
                if topic == "limiter-admit-result" && outcome == "not_sent"))),
        "the result re-enters as not_sent: {ff:?}"
    );
}

/// The state of a call whose admit a cap refused, dropping its set: it
/// holds nothing and owes its release.
fn unconfirmed(key: &str) -> CallLimiterState {
    let mut state = CallLimiterState::uncounted(key.to_string());
    state.apply_admit(&call::AdmitReport {
        key: key.to_string(),
        change: 1,
        entries: vec![call::LimiterEntry { id: "x".into(), limit: 1 }],
        outcome: call::AdmitOutcome::Rejected {
            limiter_id: "x".into(),
            held: call::LimiterHeld { change: 1, entries: vec![] },
        },
    });
    state
}
