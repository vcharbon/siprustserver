//! The message cap's teardown of a runaway call: a turn that pushes the call's
//! per-interval message count past `max_messages_per_call` ends the call
//! through the standard executor, after the turn's own event was serviced.

use b2bua_sdk::model::RuleAction;
use call::{LegState, TerminationCause};

use super::RouterCtx;
use crate::effects::HandlerResult;
use crate::rules::{ActionExecutor, RuleCall, RuleContext};

/// Tear the runaway call `result` leaves down, `turn` being the context the
/// rules read. Per-leg BYE/CANCEL, dialog-tag ownership and the safety-timer
/// contract apply exactly as a rule-driven termination would, and the turn
/// closes with the terminal invariants as a rule-driven termination does: a
/// call whose legs are all resolved here is `Terminated` — CDR written,
/// limiter released, removed — in this turn. The RFC 3326 cause rides the
/// reason (it must start with "SIP").
pub(super) fn tear_down_runaway(
    ctx: &RouterCtx,
    exec: &mut ActionExecutor,
    mut result: HandlerResult,
    turn: &RuleContext<'_>,
) -> HandlerResult {
    let cap_ctx = RuleContext {
        call: RuleCall::new(&result.call),
        call_ref: turn.call_ref,
        event: turn.event,
        source_leg_id: turn.source_leg_id,
        direction: turn.direction,
        now_ms: turn.now_ms,
        config: turn.config,
        discharged: turn.discharged,
        resolved: turn.resolved,
    };
    // An UNANSWERED a-leg (still trying/early) has no final response yet:
    // `begin_termination` assumes the firing *rule* already replied (as
    // `setup-timeout` does via RespondToALeg) and so only settles the leg's
    // disposition. The cap fires from the router, not a rule, so nobody
    // replied — without this the caller's INVITE hangs until its own
    // Timer B and the limiter slot is held until the ~32 s
    // TerminatingTimeout. Send the 503 cap cause as the caller's final so
    // the INVITE resolves now and the call terminates (decrementing the
    // limiter) immediately. An answered a-leg (confirmed) takes the BYE
    // path inside begin_termination — no response then.
    let mut cap_actions = Vec::new();
    if matches!(result.call.a_leg.state(), LegState::Trying | LegState::Early) {
        cap_actions.push(RuleAction::RespondToALeg {
            status: 503,
            reason: "Service Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        });
    }
    cap_actions.push(RuleAction::BeginTermination {
        reason: Some("SIP;cause=503;text=\"message-cap-exceeded\"".into()),
        cause: TerminationCause::MessageCap,
        by_leg: None,
    });
    let before = result.call.clone();
    let cap = exec.execute(&cap_actions, &result.call, &cap_ctx);
    result.call = cap.call;
    result.effects.critical.extend(cap.effects.critical);
    result.effects.outbound.extend(cap.effects.outbound);
    result.effects.soft.extend(cap.effects.soft);
    result.effects.buffered.extend(cap.effects.buffered);
    result.effects.fire_and_forget.extend(cap.effects.fire_and_forget);
    ctx.metrics.bump_message_cap_terminated();
    crate::turn_notes_shadow::metric("message_cap", "");
    exec.turn.notes.message_cap_terminated();
    result = crate::rules::invariants::enforce(
        &ctx.obligations,
        &before,
        crate::rules::invariants::finalize(result),
        turn.now_ms,
        crate::rules::invariants::UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
    );
    result
}
