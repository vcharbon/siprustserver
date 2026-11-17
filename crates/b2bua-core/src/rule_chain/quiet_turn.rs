//! Which turns persist quietly (ADR-0014, "a counter counts writes that change
//! the call, not progress"): the candidate a turn names after its rules ran,
//! and the class its whole effect set decides.

use call::{Call, CallModelState, Obligation, TimerType};
use sip_message::{Method, SipMessage};

use crate::effects::{
    CriticalStateEffect, HandlerResult, OutboundBody, OutboundSipEffect, QuietTurn,
};
use b2bua_sdk::event::CallEvent;

/// Name the turn's quiet candidate once the rules ran over `read`, the body
/// they read. A re-ACK whose rule also wrote into the body (a CDR event, a
/// disposition) is no candidate. An ACK that discharged nothing here and left
/// the body as the rules read it is an absorbed-ACK candidate; a final's copy
/// the count absorbed (`absorbed_copy`) that left it so, an absorbed-copy one.
pub(super) fn name_candidate(
    result: &mut HandlerResult,
    read: &Call,
    event: &CallEvent,
    (discharged, absorbed_copy): (Option<&Obligation>, bool),
) {
    if result.effects.quiet == Some(QuietTurn::ReAck) && result.call != *read {
        result.effects.quiet = None;
    }
    if received_ack(event) && discharged.is_none() && result.call == *read {
        result.effects.quiet = Some(QuietTurn::AbsorbedAck);
    }
    if absorbed_copy && result.call == *read {
        result.effects.quiet = Some(QuietTurn::AbsorbedCopy);
    }
}

/// The quiet class the turn is persisted under, or `None` for a write. An
/// author's class is a candidate; the turn's effect set decides. A quiet turn
/// is exactly: an `Active` call, no unheld answer and no soft, buffered or
/// fire-and-forget effect,
/// and for a repeat one outbound effect, it the retained datagram's repeat,
/// with no critical effect but the rung's own re-arm for
/// [`QuietTurn::OwnRung`]; a [`QuietTurn::AbsorbedAck`] or
/// [`QuietTurn::AbsorbedCopy`] has no effect at all.
/// A rule that reaches the re-ACK beside anything else — a BYE, a CDR event, a
/// timer — is a write, as is a re-ACK on a terminating call, whose flush the
/// teardown needs.
pub fn class(result: &HandlerResult) -> Option<QuietTurn> {
    let kind = result.effects.quiet?;
    let fx = &result.effects;
    let repeat = |eff: &OutboundSipEffect| matches!(eff.body, OutboundBody::Datagram(_));
    let own_effects = match (kind, fx.outbound.as_slice(), fx.critical.as_slice()) {
        (QuietTurn::ReAck, [eff], []) => repeat(eff),
        (QuietTurn::OwnRung, [eff], [CriticalStateEffect::ScheduleTimer(entry)]) => {
            repeat(eff) && entry.key.timer_type == TimerType::Rung
        }
        (QuietTurn::AbsorbedAck | QuietTurn::AbsorbedCopy, [], []) => true,
        _ => false,
    };
    let quiet = result.call.state == CallModelState::Active
        && own_effects
        && fx.unheld.is_empty()
        && fx.soft.is_empty()
        && fx.buffered.is_empty()
        && fx.fire_and_forget.is_empty();
    quiet.then_some(kind)
}

/// Whether `event` is an ACK the call received.
fn received_ack(event: &CallEvent) -> bool {
    matches!(event, CallEvent::Sip { message, .. }
        if matches!(message.as_ref(), SipMessage::Request(req) if *req.method() == Method::Ack))
}
