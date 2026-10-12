//! A dialled leg's ring deadline (`NoAnswer`, leg scope): the `no_answer` of
//! the attempt that dialled the leg, run from the first 180–189 its INVITE
//! draws. A leg that answers or fails before any never arms it. It is held
//! inside the INVITE's client-transaction bound, measured from the send, so the
//! CANCEL it fires and the 487 that CANCEL draws complete inside the live
//! transaction (RFC 3261 §9.1).

use call::{Call, LegState, TimerKey, TimerType};

use crate::config::B2buaConfig;
use crate::effects::HandlerEffects;
use crate::timer_arm::arm;

/// Arm `leg_id`'s ring deadline at its first 18x, taken at `now_ms` on the
/// INVITE that dialled it (`cseq`): `no_answer` of the leg's attempt after
/// `now_ms`, held under the transaction bound. Nothing for a leg that rang
/// before, one no attempt dialled, one already answered or being cancelled,
/// an 18x to any other INVITE, or an attempt whose ring deadline is off.
pub(crate) fn arm_at_first_18x(
    config: &B2buaConfig,
    call: &mut Call,
    fx: &mut HandlerEffects,
    leg_id: &str,
    cseq: i64,
    now_ms: i64,
) {
    let Some(leg) = call::helpers::find_b_leg(call, leg_id) else { return };
    let Some(invite) = leg.outbound_invite.as_ref() else { return };
    if leg.rang || invite.cseq != cseq || !matches!(leg.state(), LegState::Trying | LegState::Early)
    {
        return;
    }
    let Some(ms) = leg
        .attempt
        .and_then(|id| call::helpers::find_attempt(call, id))
        .and_then(|attempt| attempt.no_answer.millis())
    else {
        return;
    };
    let sent_at = invite.sent_at_ms;
    let wanted = now_ms.saturating_add(ms);
    let held = sent_at.saturating_add(config.clamp_no_answer_ms(wanted.saturating_sub(sent_at)));
    if held != wanted {
        tracing::debug!(
            call_ref = %call.call_ref,
            leg_id,
            requested_ms = ms,
            held_ms = held - now_ms,
            invite_txn_timeout_sec = config.invite_txn_timeout_sec,
            "ring deadline held under the INVITE transaction bound"
        );
    }
    arm(call, fx, TimerKey::leg(TimerType::NoAnswer, leg_id), held);
}

#[cfg(test)]
#[path = "ring_deadline_tests.rs"]
mod tests;
