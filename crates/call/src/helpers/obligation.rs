//! The books of the dialog-level retransmission obligations (ADR-0029 X4):
//! which [`Obligation`]s the call owes, the retained emission each ladder
//! repeats, the key an inbound ACK discharges, and the scopes a ladder is
//! retired under. Pure reads and value-returning writes; the timers that pace
//! a ladder live in the b2bua executor, which keys them by the same
//! [`Obligation`].

use std::time::Duration;

use crate::model::{Call, Obligation, RetainedEmission, Unacked2xx};

/// Which obligations a retirement covers.
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
}

/// Every obligation the books name inside `scope`, ladder live or not: a
/// reliable provisional whose ladder ceased still names the give-up it may
/// have left armed, so it is retired by name rather than by emission.
pub fn obligations_in(call: &Call, scope: &Scope<'_>) -> Vec<Obligation> {
    match scope {
        Scope::Obligation(o) => vec![o.clone()],
        Scope::Leg(leg_id) => unacked_2xx(call)
            .into_iter()
            .filter(|o| o.leg() == Some(leg_id))
            .chain(
                call.reliable_provisionals
                    .iter()
                    .filter(|r| {
                        r.b_leg_id == *leg_id
                            || crate::helpers::leg_shown(call, &r.a_tag) == Some(leg_id)
                    })
                    .map(|r| Obligation::PrackOf { a_tag: r.a_tag.clone(), a_rseq: r.a_rseq }),
            )
            .collect(),
        Scope::Transaction { leg_id, cseq } => call
            .reliable_provisionals
            .iter()
            .filter(|r| r.b_leg_id == *leg_id && r.b_cseq == *cseq)
            .map(|r| Obligation::PrackOf { a_tag: r.a_tag.clone(), a_rseq: r.a_rseq })
            .collect(),
        Scope::Provisionals => call
            .reliable_provisionals
            .iter()
            .map(|r| Obligation::PrackOf { a_tag: r.a_tag.clone(), a_rseq: r.a_rseq })
            .collect(),
        Scope::Call => unacked_2xx(call)
            .into_iter()
            .chain(
                call.reliable_provisionals
                    .iter()
                    .map(|r| Obligation::PrackOf { a_tag: r.a_tag.clone(), a_rseq: r.a_rseq }),
            )
            .collect(),
    }
}

/// Every 2xx the call still awaits an ACK for, as the key its ACK carries.
fn unacked_2xx(call: &Call) -> Vec<Obligation> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| {
            leg.dialogs.iter().flat_map(move |d| {
                d.ext.answered_2xx.iter().chain(d.ext.pending_reinvite_2xx.iter()).map(move |u| {
                    Obligation::AckOf2xx {
                        leg: leg.leg_id.clone(),
                        dialog_tag: u.dialog_tag.clone(),
                        cseq: u.cseq,
                    }
                })
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
/// the 2xx that confirmed the call — rather than of a re-INVITE's 2xx. Read
/// off the a-leg dialog's retained answer, which stays until that ACK arrives
/// or the call ends.
pub fn answers_initial_invite(call: &Call, obligation: &Obligation) -> bool {
    let Obligation::AckOf2xx { leg, dialog_tag, cseq } = obligation else {
        return false;
    };
    *leg == call.a_leg.leg_id
        && call.a_leg.dialogs.iter().any(|d| {
            d.ext
                .answered_2xx
                .as_ref()
                .is_some_and(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
        })
}

/// The retained emission `obligation`'s ladder repeats — `None` once the
/// ladder is discharged or has ceased.
pub fn retained_for<'a>(call: &'a Call, obligation: &Obligation) -> Option<&'a RetainedEmission> {
    match obligation {
        Obligation::AckOf2xx { .. } => unacked_2xx_slot(call, obligation).map(|u| &u.emission),
        Obligation::PrackOf { a_tag, a_rseq } => {
            crate::helpers::reliable_provisional_emission(call, a_tag, *a_rseq)
        }
    }
}

/// Step `obligation`'s ladder onto its next rung and return the wait before
/// it, or `None` when the ladder is over — no live emission, or the next rung
/// would land at or past the bound (`give_up` where the owner states its own,
/// the class's otherwise). The caller arms the rung's timer, or ceases.
pub fn advance_ladder(
    mut call: Call,
    obligation: &Obligation,
    give_up: Option<Duration>,
) -> (Call, Option<Duration>) {
    let next = match obligation {
        Obligation::AckOf2xx { .. } => {
            unacked_2xx_slot_mut(&mut call, obligation).and_then(|u| u.emission.advance(give_up))
        }
        Obligation::PrackOf { a_tag, a_rseq } => call
            .reliable_provisionals
            .iter_mut()
            .find(|r| r.a_tag == *a_tag && r.a_rseq == *a_rseq && !r.acknowledged)
            .and_then(|r| r.emission.as_mut())
            .and_then(|e| e.advance(give_up)),
    };
    (call, next)
}

/// Drop what `obligation`'s ladder repeats: the ladder is over — discharged,
/// ceased, or retired with its leg, transaction or call — so the bytes can
/// never be sent again and stop riding the replicated body. For a 2xx this is
/// also the end of the RFC 6026 *Accepted* interval the marker held.
pub fn clear_retained(mut call: Call, obligation: &Obligation) -> Call {
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
                if matches(&d.ext.answered_2xx) {
                    d.ext.answered_2xx = None;
                }
                if matches(&d.ext.pending_reinvite_2xx) {
                    d.ext.pending_reinvite_2xx = None;
                }
            }
            call
        }
        Obligation::PrackOf { a_tag, a_rseq } => {
            crate::helpers::clear_reliable_provisional_emission(call, a_tag, *a_rseq)
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
        .flat_map(|d| d.ext.answered_2xx.iter().chain(d.ext.pending_reinvite_2xx.iter()))
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
        .flat_map(|d| d.ext.answered_2xx.iter_mut().chain(d.ext.pending_reinvite_2xx.iter_mut()))
        .find(|u| u.dialog_tag == *dialog_tag && u.cseq == *cseq)
}
