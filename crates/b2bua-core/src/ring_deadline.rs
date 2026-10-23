//! A dialled leg's ring deadline (`NoAnswer`, leg scope): the `no_answer` of
//! the attempt that dialled the leg, run from the first 180–189 its INVITE
//! draws. A leg that answers or fails before any never arms it. It is held
//! inside the INVITE's client-transaction bound, measured from the send, so the
//! CANCEL it fires and the 487 that CANCEL draws complete inside the live
//! transaction (RFC 3261 §9.1).
//!
//! A call restored on another node may lack the turn that took a leg's first
//! 18x: an unanswered leg with no 18x on record runs its ring deadline from
//! its INVITE ([`arm_unrung_after_restore`]) until an 18x it takes re-arms the
//! deadline from that 18x. The leg's ring state stays as the record states
//! it: a service reads such a leg as not having rung.

use call::{Call, Leg, LegState, TimerKey, TimerType};

use crate::config::B2buaConfig;
use crate::effects::HandlerEffects;
use crate::timer_arm::arm;

/// Arm `leg_id`'s ring deadline at its first 18x, taken at `now_ms` on the
/// INVITE that dialled it (`cseq`): `no_answer` of the leg's attempt after
/// `now_ms`, held under the transaction bound. Nothing for a leg that rang
/// before, one no attempt dialled, one already answered or being cancelled,
/// an 18x to any other INVITE, or an attempt whose ring deadline is off. A leg
/// whose deadline runs from its INVITE since a restore takes its real 18x
/// here.
pub fn arm_at_first_18x(
    config: &B2buaConfig,
    call: &mut Call,
    fx: &mut HandlerEffects,
    leg_id: &str,
    cseq: i64,
    now_ms: i64,
) {
    let Some(leg) = call::helpers::find_b_leg(call, leg_id) else { return };
    let Some(invite) = leg.outbound_invite.as_ref() else { return };
    if leg.ring.rang() || invite.cseq != cseq || !rings_unanswered(leg) {
        return;
    }
    let Some(ms) = ring_ms(call, leg) else { return };
    let sent_at = invite.sent_at_ms;
    let wanted = now_ms.saturating_add(ms);
    let held = held_under_bound(config, sent_at, wanted);
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

/// Run the ring deadline of every unrung leg of a restored call from its
/// INVITE; the call's instants are already in this node's clock frame. A leg
/// whose INVITE is still unanswered and uncancelled, with no 18x on record and
/// no ring deadline armed, under an attempt whose ring deadline is on, is
/// armed from its INVITE, held under the transaction bound: the turn that took
/// its first 18x may not have reached this node. Its ring state stays
/// [`call::Ring::Silent`]; its ledger entry rides `call.timers`, which the
/// caller restores.
pub fn arm_unrung_after_restore(config: &B2buaConfig, call: &mut Call) {
    let unrung: Vec<(String, i64)> = call
        .b_legs
        .iter()
        .filter(|leg| !leg.ring.rang() && rings_unanswered(leg))
        .filter_map(|leg| {
            let invite = leg.outbound_invite.as_ref()?;
            if invite.final_received.is_some() || invite.cancel_sent {
                return None;
            }
            let key = TimerKey::leg(TimerType::NoAnswer, &leg.leg_id);
            if call.timers.iter().any(|t| t.key == key) {
                return None;
            }
            let ms = ring_ms(call, leg)?;
            let wanted = invite.sent_at_ms.saturating_add(ms);
            Some((leg.leg_id.clone(), held_under_bound(config, invite.sent_at_ms, wanted)))
        })
        .collect();
    for (leg_id, fire_at) in unrung {
        call::helpers::arm_timer(call, TimerKey::leg(TimerType::NoAnswer, &leg_id), fire_at);
    }
}

/// The leg is unanswered and not being cancelled.
fn rings_unanswered(leg: &Leg) -> bool {
    matches!(leg.state(), LegState::Trying | LegState::Early)
}

/// The ring deadline, ms, of the attempt that dialled `leg`.
fn ring_ms(call: &Call, leg: &Leg) -> Option<i64> {
    leg.attempt
        .and_then(|id| call::helpers::find_attempt(call, id))
        .and_then(|attempt| attempt.no_answer.millis())
}

/// `wanted`, held to the transaction bound less its CANCEL margin after the
/// INVITE left at `sent_at`.
fn held_under_bound(config: &B2buaConfig, sent_at: i64, wanted: i64) -> i64 {
    sent_at.saturating_add(config.clamp_no_answer_ms(wanted.saturating_sub(sent_at)))
}

#[cfg(test)]
#[path = "ring_deadline_tests.rs"]
mod tests;
