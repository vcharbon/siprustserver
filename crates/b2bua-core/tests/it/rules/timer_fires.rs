//! The timer rules' reading of a fire: the global duration's and the no-answer
//! deadline's selection and treatment, and a CANCEL or a termination retiring
//! the timers a leg guards.

use super::fixtures::*;
use super::*;

#[test]
fn timer_global_duration_selects_max_duration() {
    let call = test_call();
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("max-duration"));
}

/// The `no-answer` rule's output for a fire naming `fired_leg_id` on `call`.
fn no_answer_result(call: &call::Call, fired_leg_id: &str) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        key: TimerKey::leg(TimerType::NoAnswer, fired_leg_id),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let config = B2buaConfig::default();
    let ctx = context(call, &event, fired_leg_id, &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let no_answer = ranked.iter().find(|r| r.id == "no-answer").expect("no-answer is a candidate");
    (no_answer.handle)(&ctx).expect("no-answer handles its own timer").actions
}

#[test]
fn no_answer_fire_on_an_answered_caller_is_absorbed() {
    // The caller's INVITE took its 2xx: no final may be authored on it. The
    // fire is absorbed with no action — the fired instance already left the
    // ledger at its delivery.
    let mut call = test_call();
    confirm_a(&mut call);
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-1");
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn no_answer_fire_naming_an_absent_leg_on_an_answered_caller_is_absorbed() {
    // The caller is answered: whatever leg the fire names, no final may be
    // authored on her INVITE.
    let mut call = test_call();
    confirm_a(&mut call);
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-ghost");
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn no_answer_fire_on_own_pending_leg_fires_despite_other_leg_confirmed() {
    // The guard is per-leg, not call-level: another leg being Confirmed does
    // not spend a fire whose OWN leg is still awaiting an answer — the normal
    // body (CDR timeout + DestroyLeg on the fired leg) runs.
    let mut call = test_call();
    let mut confirmed = b_leg_pending();
    call::testkit::stage_leg(&mut confirmed, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, confirmed);
    let mut pending = b_leg_pending();
    pending.leg_id = "b-2".into();
    call::testkit::stage_leg(&mut pending, LegState::Trying);
    call = call::helpers::add_b_leg(call, pending);
    let actions = no_answer_result(&call, "b-2");
    // No consult here: the caller's 480 and the teardown follow the destroy.
    match actions.as_slice() {
        [RuleAction::AddCdrEvent { leg_id, reason, .. }, RuleAction::DestroyLeg { leg_id: destroyed, .. }, RuleAction::RespondToALeg { status: 480, .. }, RuleAction::BeginTermination { .. }]
            if leg_id == "b-2"
                && reason.as_deref() == Some("no_answer_timeout")
                && destroyed == "b-2" => {}
        other => panic!("genuine fire runs the normal no-answer body, got {other:?}"),
    }
}

/// The executor applying `actions` to `call` on a caller-side event.
fn executed(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
        at_ms: 0,
    };
    let config = B2buaConfig::default();
    let ctx = context(call, &event, "a", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    exec.execute(actions, call, &ctx)
}

#[test]
fn cancelling_a_leg_retires_its_timers() {
    // A CANCELled leg awaits no answer: its ring deadline and every other
    // timer it guards leave the ledger and the driver with the CANCEL, and
    // the call's own timers stay.
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
    let duration = TimerKey::call(TimerType::GlobalDuration);
    call::helpers::arm_timer(&mut call, no_answer.clone(), 5_000);
    call::helpers::arm_timer(&mut call, duration.clone(), 3_600_000);
    let result = executed(&call, &[RuleAction::CancelLeg { leg_id: "b-1".into() }]);
    let keys: Vec<_> = result.call.timers.iter().map(|t| t.key.clone()).collect();
    assert_eq!(keys, vec![duration], "only the call's timer stays");
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::CancelTimer { key } if *key == no_answer)),
        "the live NoAnswer fiber is cancelled, got {:?}",
        result.effects.critical,
    );
}

/// The `max-duration` rule's output for a `GlobalDuration` fire on `call`.
fn max_duration_result(call: &call::Call) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let config = B2buaConfig::default();
    let ctx = context(call, &event, "a", &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let max_duration =
        ranked.iter().find(|r| r.id == "max-duration").expect("max-duration is a candidate");
    (max_duration.handle)(&ctx).expect("max-duration handles its timer").actions
}

#[test]
fn max_duration_fire_on_a_terminating_call_is_absorbed() {
    // The cap crossing the terminating window (caller BYE'd just before it, or
    // a restored record held a stale entry): even the consult-eligible shape —
    // answered, subscribed, callback_context — is spent on a going-away call.
    // No `call_release` consult (a `reroute` outcome would dial a fresh b-leg),
    // no BeginTermination re-arm — absorbed.
    let mut call = test_call();
    confirm_a(&mut call);
    call.state = CallModelState::Terminating;
    call.callback_context = Some("cb".into());
    call.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    let actions = max_duration_result(&call);
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn max_duration_fire_on_a_live_subscribed_call_still_consults() {
    // The going-away guard is narrow: an Active answered subscribed call's cap
    // keeps the `call_release` consult.
    let mut call = test_call();
    confirm_a(&mut call);
    call.callback_context = Some("cb".into());
    call.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    let actions = max_duration_result(&call);
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::ReleaseAsyncHttp { .. })),
        "live subscribed cap consults call_release, got {actions:?}",
    );
}

#[test]
fn begin_termination_scrubs_per_leg_no_answer_entries() {
    // Entering `terminating` cancels every per-leg NoAnswer ledger entry —
    // including the entry of a leg the teardown loop skips as already
    // `Cancelling` — so the fire is stopped at the source and no restore of the
    // record brings it back into the terminating window.
    let mut call = test_call();
    let mut b = b_leg_pending();
    cancel_sent(&mut b);
    call = call::helpers::add_b_leg(call, b);
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
    call::helpers::arm_timer(&mut call, no_answer.clone(), 5_000);
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
        at_ms: 0,
    };
    let config = B2buaConfig::default();
    let ctx = context(&call, &event, "a", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let result = exec.execute(
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
        &call,
        &ctx,
    );
    assert!(
        !result.call.timers.iter().any(|t| *t.timer_type() == TimerType::NoAnswer),
        "no NoAnswer entry survives BeginTermination: {:?}",
        result.call.timers,
    );
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::CancelTimer { key } if *key == no_answer)),
        "the live NoAnswer fiber is cancelled, got {:?}",
        result.effects.critical,
    );
}
