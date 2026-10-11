//! The reliable-provisional model invariants (RFC 3262): the PRACK this
//! stack owes a responder is settled when nothing else will send it, and a
//! shown provisional's ladder holds its timers exactly while it repeats.

use call::helpers::find_leg;
use call::{Ceased, Ladder, LegDisposition, LegState, Obligation, ResponderAck, TimerType};

use super::Turn;

/// No reliable provisional's responder is still owed a PRACK once its INVITE
/// has its final, or its leg is being cancelled or has ended (RFC 3262 §4):
/// the engine settles it then. One whose offer waits for the ACK is owed only
/// while its dialog is confirmed and awaits that ACK.
pub(super) fn no_owed_prack_past_its_invite(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    for p in call.provisionals.iter().filter(|p| p.responder.is_owed()) {
        let leg = find_leg(call, &p.id.leg);
        let held = if p.responder == ResponderAck::DeferredToAck {
            leg.and_then(|l| l.dialogs.confirmed())
                .is_some_and(|d| d.peer_tag == p.id.responder_tag && d.awaited_ack_cseq.is_some())
        } else {
            let ended = leg.is_none_or(|l| {
                l.state() == LegState::Terminated || l.disposition() == LegDisposition::Cancelling
            });
            !ended && crate::rules::actions::invite_open(call, &p.id)
        };
        if !held {
            return Err(format!(
                "leg {} ({:?}): reliable 1xx RSeq {} of INVITE CSeq {} on dialog {} is still \
                 owed its PRACK ({:?})",
                p.id.leg,
                leg.map(|l| (l.state(), l.disposition())),
                p.id.rseq.0,
                p.id.invite_cseq.0,
                p.id.responder_tag,
                p.responder,
            ));
        }
    }
    Ok(())
}

/// No reliable provisional shown to no one is still owed its PRACK at the
/// end of a turn (RFC 3262 §4): no party will PRACK it, so the engine sends
/// this stack's in the turn it arrived.
pub(super) fn no_unshown_owed_prack(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    match call
        .provisionals
        .iter()
        .find(|p| p.shown.is_none() && p.responder == ResponderAck::Pending)
    {
        Some(p) => Err(format!(
            "leg {}: reliable 1xx RSeq {} of INVITE CSeq {} on dialog {}, shown to no one, is \
             still owed its PRACK",
            p.id.leg, p.id.rseq.0, p.id.invite_cseq.0, p.id.responder_tag
        )),
        None => Ok(()),
    }
}

/// A shown provisional's ladder holds its timers exactly while it repeats
/// (RFC 3262 §3): repeating, its rung and its give-up are armed; ceased at its
/// last rung, its give-up alone stands; unsent, acknowledged or ceased with
/// its final, no timer of its ladder is armed.
pub(super) fn shown_ladder_armed_while_repeating(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    let armed = |o: &Obligation, ty: TimerType| {
        call.timers.iter().any(|t| t.key.timer_type == ty && t.key.obligation_of() == Some(o))
    };
    for s in call.provisionals.iter().filter_map(|p| p.shown.as_ref()) {
        let o = Obligation::AwaitPrack { tag: s.tag.clone(), rseq: s.rseq };
        let (rung, give_up) = (armed(&o, TimerType::Rung), armed(&o, TimerType::RepeatGiveUp));
        let held = match s.ladder {
            Ladder::Repeating(_) => rung && give_up,
            Ladder::Ceased(Ceased::GiveUp) => !rung,
            Ladder::Unsent | Ladder::Acknowledged | Ladder::Ceased(Ceased::Final) => {
                !rung && !give_up
            }
        };
        if !held {
            let state = match &s.ladder {
                Ladder::Repeating(_) => "repeating".to_string(),
                other => format!("{other:?}"),
            };
            return Err(format!(
                "shown 1xx {o:?} is {state} with rung armed {rung} and give-up armed {give_up}"
            ));
        }
    }
    Ok(())
}
