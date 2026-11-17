//! The CORE_LAYER rules that handle the call reaper's verdicts, and their
//! bodies (ADR-0020 X1): a call found dead is forced terminal with no wire
//! message; a call whose legs are alive — condemned for its overflow, its
//! message cap or its Call-ID's message budget, or whose handler panicked —
//! is torn down on the wire while Active.

use b2bua_sdk::config::Cap;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{LegEnd, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult};
use call::{CallModelState, CdrEventType, TerminationCause};

use super::core_rules::rule;

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

/// The `reaper-message-budget` body: a Call-ID the call holds passed its
/// message budget and is muted (ADR-0063 §4). An Active call's live legs are
/// torn down on the wire, the muted leg's messages dropped by the transaction
/// layer; a terminating call is forced terminal now, its cause kept where one
/// is named.
pub(super) fn reap_message_budget(ctx: &RuleContext) -> Option<RuleHandleResult> {
    const REASON: &str = "message-budget";
    match ctx.call.state() {
        CallModelState::Active => {
            teardown_on_the_wire(ctx, REASON, TerminationCause::MessageBudget)
        }
        _ => force_terminal(ctx, REASON, TerminationCause::MessageBudget),
    }
}

/// The `reaper-fatal-error` body: a turn of the call panicked, hung or
/// crossed a per-call cap (ADR-0057 §4), its legs are alive. An Active call's
/// live legs are torn down on the wire ([`teardown_on_the_wire`]); a
/// terminating call's peers were already sent their teardown, so it is
/// forced terminal now. The CDR names the cap a trip crossed, which never
/// reaches the wire.
pub(super) fn reap_handler_panic(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let reason = tripped_cap(ctx).map_or("handler-panic", Cap::cdr_reason);
    match ctx.call.state() {
        CallModelState::Active => teardown_on_the_wire(ctx, reason, TerminationCause::Supervisor),
        _ => force_terminal(ctx, reason, TerminationCause::Supervisor),
    }
}

/// The cap a verdict's tripped turn crossed, from its payload.
fn tripped_cap(ctx: &RuleContext) -> Option<Cap> {
    let CallEvent::InternalEvent { payload, .. } = ctx.event else { return None };
    payload.get("cap").and_then(|c| c.as_str()).and_then(Cap::of_label)
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

/// The CORE_LAYER rules of the call reaper's verdicts (ADR-0020 X1/X6).
///
/// The reaper's verdicts arrive as ordinary InternalEvents and are
/// handled by ordinary CORE rules — the single funnel. `stale` forces
/// every unresolved leg terminal with no wire traffic (the call is
/// provably dead: its stamp froze past the idle threshold) and runs
/// while terminating: a wedged terminating call is exactly what it
/// reaps. `overflow` tears the live legs down on the wire and leaves a
/// terminating call to its teardown. `fatal-error` (a handler panicked,
/// the legs are alive), `message-cap` (the work bound, in every
/// state) and `message-budget` (a muted Call-ID, ADR-0063) do either,
/// as the call's state asks: an Active call is torn down on the wire,
/// a terminating one forced terminal. Each records
/// the reason and begins termination; the
/// invariant then promotes → Terminated and discharges the obligations
/// (CDR + limiter) exactly once. The `discharge` outcome deliberately
/// has NO rule (the router's bypass branch owns it — rules are the
/// thing that failed by then).
pub(super) fn reaper_verdict_rules() -> [RuleDefinition; 5] {
    [
        rule(
            "reaper-stale",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_STALE),
            reap_stale,
        )
        .runs_while_terminating(),
        rule(
            "reaper-fatal-error",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_FATAL),
            reap_handler_panic,
        )
        .runs_while_terminating(),
        rule(
            "reaper-overflow",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_OVERFLOW),
            reap_overflow,
        ),
        rule(
            "reaper-message-cap",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_MESSAGE_CAP),
            reap_message_cap,
        )
        .runs_while_terminating(),
        rule(
            "reaper-message-budget",
            &[],
            Match::internal_event()
                .topic(crate::reaper_verdict::REAPER_TOPIC)
                .outcome(crate::reaper_verdict::OUTCOME_MESSAGE_BUDGET),
            reap_message_budget,
        )
        .runs_while_terminating(),
    ]
}
