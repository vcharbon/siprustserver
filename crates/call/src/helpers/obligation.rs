//! The books of the dialog-level retransmission obligations (ADR-0032 X4):
//! which [`Obligation`]s the call owes, the retained emission each ladder
//! repeats, the key an inbound ACK discharges, and the scopes a ladder is
//! retired under. Pure reads and value-returning writes; the timers that pace
//! a ladder live in the b2bua executor, which keys them by the same
//! [`Obligation`].

use std::time::Duration;

use crate::model::{Call, Ceased, Ladder, Obligation, Provisional, RetainedEmission, Unacked2xx};

/// What a retirement covers: the obligations it ends, and for a leg, an
/// attempt, a service machine or the call, the timers that guard it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Exactly this one — the discharging ACK or PRACK arrived, or a spent
    /// rung found nothing left to repeat.
    Obligation(Obligation),
    /// Everything `leg_id` owes or was shown: its un-ACKed 2xx, and every
    /// reliable provisional it raised or that was shown on it — a torn-down
    /// leg has no one left to reach, and a rung re-offering its answer would
    /// solicit a PRACK toward a corpse (RFC 3262 §3). Every other leg's
    /// ladder keeps running (§4, errata 4603).
    Leg(&'a str),
    /// The reliable provisionals answering the INVITE transaction `cseq` on
    /// the responder leg `leg_id` — the one whose final has now reached, or
    /// been answered on, the face they were shown (RFC 3262 §3). A re-INVITE's
    /// final leaves every other ladder alone.
    Transaction { leg_id: &'a str, cseq: i64 },
    /// Every reliable provisional the call has shown, on either face — a final
    /// on the a-leg INVITE ends every early-dialog provisional at once (RFC
    /// 3261 §17.2.1). A 2xx awaiting its ACK is not the setup's and stays: the
    /// b-leg re-INVITE 2xx still owed an ACK keeps its ladder and its RFC 6026
    /// *Accepted* marker.
    Provisionals,
    /// Every obligation on the call — a terminating call repeats nothing.
    Call,
    /// The timers guarding one attempt — an ended attempt owes no
    /// retransmission of its own.
    Attempt(crate::model::AttemptId),
    /// The timers a service machine owns — a deactivated machine owes no
    /// retransmission of its own.
    Machine(&'a crate::model::MachineId),
}

/// Every obligation the books name inside `scope`, ladder live or not: a 2xx
/// whose ladder ceased at its last rung still names the give-up it left
/// armed, so it is retired by name rather than by emission.
pub fn obligations_in(call: &Call, scope: &Scope<'_>) -> Vec<Obligation> {
    match scope {
        Scope::Obligation(o) => vec![o.clone()],
        Scope::Leg(leg_id) => unacked_2xx(call)
            .into_iter()
            .filter(|o| o.leg() == Some(leg_id))
            .chain(awaited_pracks(call, |p, tag| {
                p.id.leg == *leg_id || crate::helpers::leg_shown(call, tag) == Some(leg_id)
            }))
            .collect(),
        Scope::Transaction { leg_id, cseq } => {
            awaited_pracks(call, |p, _| p.id.leg == *leg_id && p.id.invite_cseq.as_i64() == *cseq)
        }
        Scope::Provisionals => awaited_pracks(call, |_, _| true),
        Scope::Attempt(_) | Scope::Machine(_) => Vec::new(),
        Scope::Call => {
            unacked_2xx(call).into_iter().chain(awaited_pracks(call, |_, _| true)).collect()
        }
    }
}

/// The PRACK obligation of every shown provisional `keep` selects, by the
/// record and the tag it was shown under.
fn awaited_pracks(call: &Call, keep: impl Fn(&Provisional, &str) -> bool) -> Vec<Obligation> {
    call.provisionals
        .iter()
        .filter_map(|p| p.shown.as_ref().map(|s| (p, s)))
        .filter(|(p, s)| keep(p, &s.tag))
        .map(|(_, s)| Obligation::AwaitPrack { tag: s.tag.clone(), rseq: s.rseq })
        .collect()
}

/// Every 2xx the call still awaits an ACK for, as the key its ACK carries.
fn unacked_2xx(call: &Call) -> Vec<Obligation> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| {
            leg.dialogs.iter().filter_map(crate::helpers::answered_2xx).map(move |u| {
                Obligation::AckOf2xx {
                    leg: leg.leg_id.clone(),
                    dialog_tag: u.dialog_tag.clone(),
                    cseq: u.cseq,
                }
            })
        })
        .collect()
}

/// The key an ACK arriving on `leg_id` with `dialog_tag` and `cseq`
/// discharges — `Some` only when the books hold a 2xx awaiting exactly that
/// ACK (RFC 3261 §13.3.1.4). A retransmitted initial ACK names no pending
/// re-INVITE 2xx, and a b-leg ACK never names the a-leg's answer.
pub fn acked_2xx(call: &Call, leg_id: &str, dialog_tag: &str, cseq: i64) -> Option<Obligation> {
    let key =
        Obligation::AckOf2xx { leg: leg_id.to_string(), dialog_tag: dialog_tag.to_string(), cseq };
    unacked_2xx_slot(call, &key).map(|_| key)
}

/// Whether `obligation` is the caller's ACK of the a-leg's INITIAL answer —
/// the 2xx that confirmed the call — rather than of a re-INVITE's 2xx: the
/// a-leg retains a 2xx to her dialog-creating INVITE's CSeq, until that ACK
/// arrives or the call ends.
pub fn answers_initial_invite(call: &Call, obligation: &Obligation) -> bool {
    let Obligation::AckOf2xx { leg, .. } = obligation else {
        return false;
    };
    *leg == call.a_leg.leg_id
        && unacked_2xx_slot(call, obligation)
            .is_some_and(|u| u.cseq == i64::from(call.a_leg_invite.cseq))
}

/// The retained emission `obligation`'s ladder repeats — `None` once the
/// ladder is discharged or has ceased.
pub fn retained_for<'a>(call: &'a Call, obligation: &Obligation) -> Option<&'a RetainedEmission> {
    match obligation {
        Obligation::AckOf2xx { .. } => unacked_2xx_slot(call, obligation).map(|u| &u.emission),
        Obligation::AwaitPrack { tag, rseq } => crate::helpers::shown_emission(call, tag, *rseq),
    }
}

/// Step `obligation`'s ladder onto its next rung and return the wait before
/// it, or `None` when the ladder is over — no live emission, or the next rung
/// would land at or past the bound (`give_up` where the owner states its own,
/// the class's otherwise). The caller arms the rung's timer, or ceases.
pub fn advance_ladder(
    call: &mut Call,
    obligation: &Obligation,
    give_up: Option<Duration>,
) -> Option<Duration> {
    match obligation {
        Obligation::AckOf2xx { .. } => {
            unacked_2xx_slot_mut(call, obligation).and_then(|u| u.emission.advance(give_up))
        }
        Obligation::AwaitPrack { tag, rseq } => {
            crate::helpers::advance_shown_ladder(call, tag, *rseq, give_up)
        }
    }
}

/// Drop what `obligation`'s ladder repeats: the ladder is over — discharged,
/// or retired with its leg, transaction or call — so the bytes can never be
/// sent again and stop riding the replicated body. For a 2xx this is also the
/// end of the RFC 6026 *Accepted* interval the marker held, and the BYE held
/// behind its ACK leaves the dialog's queue (its releaser sends it); a reliable
/// provisional's ladder ceases with the final it was shown before, an
/// acknowledged one standing.
pub fn clear_retained(call: &mut Call, obligation: &Obligation) {
    match obligation {
        Obligation::AckOf2xx { leg, dialog_tag, cseq } => {
            let matches = |u: &Option<Unacked2xx>| {
                u.as_ref().is_some_and(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
            };
            for d in std::iter::once(&mut call.a_leg)
                .chain(call.b_legs.iter_mut())
                .filter(|l| l.leg_id == *leg)
                .flat_map(|l| l.dialogs.iter_mut())
            {
                if matches(&crate::helpers::answered_2xx(d).cloned()) {
                    d.drop_answered_invite();
                }
            }
        }
        Obligation::AwaitPrack { tag, rseq } => {
            crate::helpers::cease_shown_ladder(call, tag, *rseq, Ceased::Final);
        }
    }
}

/// The retained 2xx `obligation` names, on the leg's dialog that holds it.
fn unacked_2xx_slot<'a>(call: &'a Call, obligation: &Obligation) -> Option<&'a Unacked2xx> {
    let Obligation::AckOf2xx { leg, dialog_tag, cseq } = obligation else {
        return None;
    };
    crate::helpers::find_leg(call, leg)?
        .dialogs
        .iter()
        .filter_map(crate::helpers::answered_2xx)
        .find(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
}

fn unacked_2xx_slot_mut<'a>(
    call: &'a mut Call,
    obligation: &Obligation,
) -> Option<&'a mut Unacked2xx> {
    let Obligation::AckOf2xx { leg, dialog_tag, cseq } = obligation else {
        return None;
    };
    std::iter::once(&mut call.a_leg)
        .chain(call.b_legs.iter_mut())
        .find(|l| l.leg_id == *leg)?
        .dialogs
        .iter_mut()
        .filter_map(crate::helpers::answered_2xx_mut)
        .find(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
}

/// Rebind every retained copy of a message in `call` whose bytes are `was`
/// to `now` — the retained emissions ([`RetainedEmission::restate`]), the
/// caller's non-2xx final and latest provisional included, and the
/// images of the INVITEs this side sent: the message they copy left restated.
pub fn restate_retained(call: &mut Call, was: &[u8], now: &[u8]) {
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        restate_leg_invite(leg, was, now);
        if let Some(received) = leg.invite.as_mut().and_then(|r| r.invite.as_mut()) {
            for sent in received.rejection.iter_mut().chain(received.provisional.iter_mut()) {
                sent.restate(was, now);
            }
        }
        for dialog in &mut leg.dialogs {
            if let Some(unacked) = crate::helpers::answered_2xx_mut(dialog) {
                unacked.emission.restate(was, now);
            }
            if let Some(sent) = dialog.sent_invite.as_mut().and_then(|r| r.invite.as_mut()) {
                for ack in sent.emitted_ack.iter_mut().chain(sent.ack_ahead.iter_mut()) {
                    ack.restate(was, now);
                }
            }
        }
    }
    for shown in call.provisionals.iter_mut().filter_map(|p| p.shown.as_mut()) {
        if let Ladder::Repeating(emission) = &mut shown.ladder {
            emission.restate(was, now);
        }
    }
}

/// Rebind the images of `leg`'s INVITEs holding `was` to `now`: the INVITE
/// they hold left restated.
pub fn restate_leg_invite(leg: &mut crate::model::Leg, was: &[u8], now: &[u8]) {
    let images = leg
        .invite
        .iter_mut()
        .chain(leg.dialogs.iter_mut().filter_map(|d| d.sent_invite.as_mut()))
        .filter_map(|r| r.sent.as_mut());
    for sent in images {
        if sent.image == was {
            sent.image = now.to_vec();
        }
    }
}
