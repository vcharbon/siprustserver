//! The invariants a turn's result is held to (ADR-0020): the termination
//! record, the cleanup on `Terminated`, the unanswered caller's synthesized
//! final, and the global call machine's projection (ADR-0016).

use super::fixtures::*;
use super::*;

/// A terminal that names no cause is a writer the funnel does not know: a
/// defect noted on the turn, counted, and the call still cleaned up.
#[test]
fn a_terminal_with_no_termination_record_is_a_defect() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    let mut notes = TurnNotes::default();
    let leave = invariants::UnansweredCaller::Leave;
    let core = ObligationSet::core();
    let result =
        invariants::enforce(&core, &before, HandlerResult::new(call), 0, leave, &mut notes);
    let defects: Vec<_> = notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [b2bua_core::turn_notes::TERMINATION_RECORDED]);
    assert!(result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::RemoveCall)));
}

#[test]
fn invariants_append_cleanup_on_terminated() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
    );

    assert!(
        result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)),
        "cancel-all-timers guaranteed"
    );
    assert!(
        result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr)),
        "write-cdr guaranteed"
    );
    assert!(
        matches!(result.effects.critical.last(), Some(CriticalStateEffect::RemoveCall)),
        "remove-call runs last"
    );
    // ADR-0022: `before` entered the turn with the a-leg still Trying and the
    // turn answered nothing — the funnel appends the forgotten final (503) and
    // records it on the CDR.
    assert!(
        result.effects.outbound.iter().any(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() == 503)
        }),
        "unanswered a-leg gets the synthesized 503"
    );
    assert!(
        result.call.cdr_events.iter().any(|e| {
            e.reason.as_deref() == Some("unanswered_at_termination") && e.status_code == Some(503)
        }),
        "the synthesized final is on the CDR"
    );
}

/// The funnel's 503 to a caller a terminating call leaves unanswered is a
/// final minted in the element's own name: it carries the deployment's
/// advertisement for one.
#[test]
fn the_unanswered_callers_503_carries_the_deployment_advertisement() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let policy = CapabilitySet::stating(
        None,
        None,
        Some(vec![sip_message::header::AcceptRange::new("application/sdp")]),
    );
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&policy),
        &mut TurnNotes::default(),
    );
    let accept: Vec<String> = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) if r.status() == 503 => {
                Some(r.raw(HeaderName::Accept).map(str::to_string).collect())
            }
            _ => None,
        })
        .expect("the synthesized 503");
    assert_eq!(accept, ["application/sdp"]);
}

#[test]
fn no_synthesized_final_when_the_turn_already_answered() {
    // The reject path: same terminated transition, but THIS turn carries a
    // final to the a-leg (as `reject_call` / `RespondToALeg` do) — the funnel
    // must not double-answer.
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let a_invite = crate::caller_of(&call);
    let mut result = HandlerResult::new(call);
    let effect = b2bua_core::rules::relay::response_to_a_leg(
        &mut result.call,
        &mut result.effects,
        &a_invite,
        486,
        "Busy Here",
        Some("totag-x".into()),
        None,
        vec![],
        None,
        None,
        vec![],
        b2bua_core::effects::Provenance::Authored,
        b2bua_core::rules::relay::Author::Stack,
    )
    .expect("the transaction's first final is admitted");
    result.effects.outbound.push(effect);
    let final_sent = result.call.a_leg.invite_final_sent();
    assert_eq!(final_sent, Some(486), "the final is recorded on the leg");
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        result,
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
    );
    let finals_to_a = result
        .effects
        .outbound
        .iter()
        .filter(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() >= 200)
        })
        .count();
    assert_eq!(finals_to_a, 1, "exactly the rule's own final — no synthesized duplicate");
}

#[test]
fn no_synthesized_final_when_the_txn_layer_answered_the_cancel() {
    // The CANCEL path: sip-txn sent the 487 itself, so the a-leg reads Early
    // through the whole terminating window and the only trace of the final is
    // the leg fact the `Cancelled` turn recorded. The funnel reads that fact —
    // no per-turn scan, no CDR event — and answers nothing.
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    call = call::helpers::record_invite_final(call, "a", 487);
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
    );
    assert!(
        result.effects.outbound.is_empty(),
        "an a-leg carrying its 487 is not answered again: {:?}",
        result.effects.outbound
    );
    assert!(
        !result
            .call
            .cdr_events
            .iter()
            .any(|e| e.reason.as_deref() == Some("unanswered_at_termination")),
        "no synthesized-final CDR event either"
    );
    assert_eq!(result.call.a_leg.invite_final_sent(), Some(487));
}

#[test]
fn global_call_cursor_projects_lifecycle_states() {
    let cursor = |r: &HandlerResult| {
        r.call
            .sm_cursors
            .get(&invariants::GLOBAL_CALL_MACHINE)
            .map(StateLabel::as_str)
            .map(str::to_string)
    };
    let mut call = test_call();

    // Active call → "Active".
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(cursor(&r).as_deref(), Some("Active"));

    // Terminating with a still-confirmed a-leg (unresolved → no promotion)
    // → "Terminating".
    call.state = CallModelState::Terminating;
    confirm_a(&mut call);
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(r.call.state, CallModelState::Terminating, "not promoted while unresolved");
    assert_eq!(cursor(&r).as_deref(), Some("Terminating"));

    // Terminated → "Terminated".
    call.state = CallModelState::Terminated;
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(cursor(&r).as_deref(), Some("Terminated"));
}
