//! The bodies of the CORE_LAYER rules that handle the call reaper's verdicts
//! (ADR-0020 X1): a call found dead is forced terminal with no wire message;
//! a call whose legs are alive — condemned for its overflow or its message
//! cap, or whose handler panicked — is torn down on the wire while Active.

use b2bua_sdk::model::{LegEnd, RuleAction, RuleContext, RuleHandleResult};
use call::{CallModelState, CdrEventType, TerminationCause};

/// The `reaper-overflow` body. The call is condemned for flooding its
/// dispatch overflow, not found dead: its legs are alive, so they are torn
/// down on the wire ([`teardown_on_the_wire`]). It does not run on a call
/// already terminating.
pub(super) fn reap_overflow(ctx: &RuleContext) -> Option<RuleHandleResult> {
    teardown_on_the_wire(ctx, "dispatch-overflow", TerminationCause::Supervisor)
}

/// The `reaper-message-cap` body: the call crossed its lifetime message cap.
/// An Active call's live legs are torn down on the wire; a terminating
/// call's peers were already sent their BYE, so it is forced terminal now.
pub(super) fn reap_message_cap(ctx: &RuleContext) -> Option<RuleHandleResult> {
    const REASON: &str = "message-cap-lifetime";
    match ctx.call.state() {
        CallModelState::Active => teardown_on_the_wire(ctx, REASON, TerminationCause::MessageCap),
        _ => force_terminal(ctx, REASON, TerminationCause::MessageCap),
    }
}

/// The `reaper-fatal-error` body: a handler of the call panicked, its legs
/// are alive. An Active call's live legs are torn down on the wire
/// ([`teardown_on_the_wire`]); a terminating call's peers were already sent
/// their teardown, so it is forced terminal now.
pub(super) fn reap_handler_panic(ctx: &RuleContext) -> Option<RuleHandleResult> {
    const REASON: &str = "handler-panic";
    match ctx.call.state() {
        CallModelState::Active => teardown_on_the_wire(ctx, REASON, TerminationCause::Supervisor),
        _ => force_terminal(ctx, REASON, TerminationCause::Supervisor),
    }
}

/// The `reaper-stale` body, for a call found dead: force every
/// still-unresolved leg terminal (mirroring `is_fully_resolved`, like
/// `terminating-safety-timeout`), record the reason on the CDR, and command
/// termination. No wire messages: the legs were force-resolved above, so
/// `BeginTermination` skips them all and just moves the lifecycle — finalize
/// promotes, the invariant discharges the obligations.
pub(super) fn reap_stale(ctx: &RuleContext) -> Option<RuleHandleResult> {
    force_terminal(ctx, "reaper-stale", TerminationCause::Supervisor)
}

/// End a call whose legs are alive: the reason on the CDR, then the ordinary
/// teardown — BYE to each confirmed leg, CANCEL to each early b-leg, the
/// final an unanswered a-leg is owed — with `TerminatingTimeout` bounding
/// the wait for their answers.
fn teardown_on_the_wire(
    ctx: &RuleContext,
    reason: &'static str,
    cause: TerminationCause,
) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: ctx.call.a_leg().leg_id.clone(),
            status_code: None,
            reason: Some(reason.into()),
        },
        RuleAction::BeginTermination { reason: Some(reason.into()), cause, by_leg: None },
    ]))
}

/// Force every still-unresolved leg terminal, record `reason` on the CDR and
/// command termination with `cause` (kept only where the call names no cause
/// yet: the first termination names who ended the call).
fn force_terminal(
    ctx: &RuleContext,
    reason: &'static str,
    cause: TerminationCause,
) -> Option<RuleHandleResult> {
    let mut actions = Vec::new();
    for leg in std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()) {
        // `leg_is_resolved` also treats a still-`Cancelling` leg as unresolved, so
        // a wedged in-flight CANCEL is force-terminated here too (TerminateLeg
        // clears the `Cancelling` disposition, letting the call finalize).
        if !call::helpers::leg_is_resolved(leg) {
            actions.push(RuleAction::TerminateLeg {
                leg_id: leg.leg_id.clone(),
                end: LegEnd::Abandon,
            });
        }
    }
    actions.push(RuleAction::AddCdrEvent {
        event_type: CdrEventType::Bye,
        leg_id: ctx.call.a_leg().leg_id.clone(),
        status_code: None,
        reason: Some(reason.into()),
    });
    actions.push(RuleAction::BeginTermination { reason: Some(reason.into()), cause, by_leg: None });
    Some(RuleHandleResult::new(actions))
}
