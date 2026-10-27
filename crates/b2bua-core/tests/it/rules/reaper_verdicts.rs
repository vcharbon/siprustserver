//! The reaper's verdicts as the rules read them: an overflow verdict tears
//! the call down on the wire, the message-cap verdict bounds its work.

use super::fixtures::*;
use super::*;

/// The requests `result` sent toward `leg` with `method`.
fn requests_to(result: &HandlerResult, leg: &str, method: &str) -> usize {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .filter(
            |e| matches!(&e.body, b2bua_core::effects::OutboundBody::Request(r) if r.method() == method),
        )
        .count()
}

/// The reaper's `outcome` verdict for `call`.
fn reaper_verdict(call: &call::Call, outcome: &str) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: b2bua_core::reaper_verdict::REAPER_TOPIC.into(),
        outcome: outcome.into(),
        payload: serde_json::json!({}),
        body: Vec::new(),
        incarnation: None,
    }
}

/// `call` run through the default rules on the `overflow` verdict.
fn on_overflow_verdict(call: &call::Call) -> HandlerResult {
    on_verdict(call, b2bua_core::reaper_verdict::OUTCOME_OVERFLOW)
}

/// `call` run through the default rules on the reaper's `outcome` verdict.
fn on_verdict(call: &call::Call, outcome: &str) -> HandlerResult {
    let event = reaper_verdict(call, outcome);
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let ctx = ctx_for(call, &event, &config);
    execute_rules(&default_rules(), call, &ctx, &mut exec, &ObligationSet::core())
}

/// A call whose b-leg rings on an INVITE its CANCEL can name.
fn ringing_call() -> call::Call {
    let mut leg = b_leg_pending();
    // The INVITE this leg is ringing on, which its CANCEL names (RFC 3261 §9.1).
    let dest = call::HostPort { host: "10.0.0.2".into(), port: 5070 };
    let image = invite().image().to_vec();
    let sent = call::SentRequest { branch: "z9hG4bK-b1".into(), image, destination: dest };
    call::testkit::dial_with(&mut leg, sent);
    let call = call::helpers::add_b_leg(test_call(), leg);
    let (_, actions) = handle_from_b1(&call, &b_leg_ringing_event(false));
    let ringing = execute_from_b1(&call, &b_leg_ringing_event(false), &actions).call;
    assert_eq!(ringing.b_legs[0].state(), LegState::Early);
    ringing
}

/// A ringing call condemned for flooding its overflow: its callee is alive
/// and is CANCELed, and the CDR names the teardown.
#[test]
fn an_overflow_verdict_on_a_ringing_call_cancels_the_callee() {
    let result = on_overflow_verdict(&ringing_call());
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    assert!(
        result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "the CDR names the teardown"
    );
}

/// A call already terminating — bob hung up — is left to that teardown: the
/// verdict records nothing, keeps the cause, and re-arms no timer.
#[test]
fn an_overflow_verdict_on_a_terminating_call_changes_nothing() {
    let call = call::helpers::add_b_leg(test_call(), b_leg_pending());
    let mut ending =
        call::helpers::record_termination(call, 0, TerminationCause::RemoteBye, Some("b-1".into()));
    ending.state = CallModelState::Terminating;

    let result = on_overflow_verdict(&ending);
    assert!(
        !result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "no overflow teardown is recorded"
    );
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the cause bob gave stays"
    );
    assert!(
        !result.effects.critical.iter().any(|e| matches!(
            e,
            CriticalStateEffect::ScheduleTimer(t)
                if *t.timer_type() == TimerType::TerminatingTimeout
        )),
        "the terminating timer is not re-armed"
    );
}

/// An Active call past its lifetime message cap is torn down on the wire:
/// the ringing callee is CANCELed, the cause is the cap.
#[test]
fn a_message_cap_verdict_on_a_ringing_call_cancels_the_callee() {
    let result = on_verdict(&ringing_call(), b2bua_core::reaper_verdict::OUTCOME_MESSAGE_CAP);
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::MessageCap)
    );
    assert!(result
        .call
        .cdr_events
        .iter()
        .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
}

/// A terminating call past its cap — its peers already sent their BYE — is
/// forced terminal now: its unresolved legs resolved, the reason recorded,
/// the cause it already had kept.
#[test]
fn a_message_cap_verdict_on_a_terminating_call_forces_it_terminal() {
    let mut ending = call::helpers::record_termination(
        ringing_call(),
        0,
        TerminationCause::RemoteBye,
        Some("b-1".into()),
    );
    ending.state = CallModelState::Terminating;
    let result = on_verdict(&ending, b2bua_core::reaper_verdict::OUTCOME_MESSAGE_CAP);
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 0, "nothing more on the wire");
    assert!(result.call.b_legs.iter().all(call::helpers::leg_is_resolved));
    assert!(result
        .call
        .cdr_events
        .iter()
        .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the first termination names who ended the call"
    );
}
