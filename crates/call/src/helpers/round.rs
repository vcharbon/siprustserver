//! The re-INVITE rounds two copies of one call share, and how one copy's view
//! of a round lands in the other (ADR-0031, "a re-INVITE round is a third
//! chain").
//!
//! A round is the INVITE client transaction one dialog relays toward its peer,
//! named by the leg, the dialog's two tags and the transaction's branch, and
//! ordered on its dialog by the CSeq it went out with. It goes open (its relay
//! awaits a final) → answered (a 2xx taken, the originator's ACK awaited) →
//! ACKed, or ends on a non-2xx, and never goes back. Its final reaches the
//! node holding the client transaction, so a round's end another copy saw is
//! progress a record holding the round unfinished cannot make itself; so is a
//! round's opening the record never reached, whatever stage the other copy
//! holds it at. Rounds are read per dialog, from the transaction and its ACK
//! state; a dialog's sequence speaks only for a round a non-2xx ended, which
//! leaves no transaction behind.

use crate::model::{
    Call, Dialog, DialogId, Leg, Obligation, RelayOwner, RequestRecord, TimerEntry,
};

use super::invite_records::{ack_branch, awaited_ack_cseq, invite_image};
use super::leg::{every_leg, find_leg};
use super::lens::update_leg;
use super::peering::relay_peer_dialog;

/// Does `incoming` stand further along a round `held` holds unfinished, on
/// any dialog? Further along an open round: `incoming`'s same dialog no
/// longer holds the relay at the round's CSeq, and holds the same transaction
/// (it took the round's 2xx), a later one (a next round began), or none and a
/// sequence at or past the round's (a non-2xx ended it). Further along an
/// answered round: `incoming` no longer awaits that 2xx's ACK, and either the
/// same transaction's ACK left or a later transaction stands on the dialog.
pub fn round_further(incoming: &Call, held: &Call) -> bool {
    !rounds_ended(incoming, held).is_empty()
}

/// Does `incoming` hold a round `held` never reached, on a dialog both hold?
/// `incoming`'s dialog holds an INVITE client transaction, and `held`'s same
/// dialog holds neither that transaction nor a later one, nor — holding none
/// at all — a sequence at or past its CSeq (the trace a refused round leaves).
/// The round is open, answered or ACKed in `incoming`.
pub fn round_opened(incoming: &Call, held: &Call) -> bool {
    !rounds_opened(incoming, held).is_empty()
}

/// Does `incoming` carry a round's progress `held` lacks: the end of a round
/// `held` holds unfinished ([`round_further`]) or the opening of one it never
/// reached ([`round_opened`])?
pub fn round_ahead(incoming: &Call, held: &Call) -> bool {
    round_further(incoming, held) || round_opened(incoming, held)
}

/// Does `held` — a record, or none — hold every re-INVITE round `other` holds
/// open on a relay the worker `by` made: the same transaction or a later one,
/// on the same dialog? The open round's final returns to the node that
/// relayed it (RFC 3261 §18.2.2). An answered round's final is taken; its ACK
/// follows the route set, not the node, and the 2xx's retransmissions are
/// facts of the record ([`round_ahead`] reads a record behind on them). Read
/// from the transactions alone: a dialog's sequence, which every fold and
/// refusal raises, says nothing of a round's presence.
pub fn holds_rounds_of(held: Option<&Call>, other: &Call, by: &RelayOwner) -> bool {
    every_leg(other).all(|other_leg| {
        let held_leg = held.and_then(|h| find_leg(h, &other_leg.leg_id));
        other_leg.dialogs.iter().all(|o| {
            let Some(round) = unfinished(other_leg, o) else { return true };
            let Stage::Open { outbound_cseq } = round.stage else { return true };
            if !invite_relays(o).any(|p| p.outbound_cseq == outbound_cseq && p.relayed_by == *by) {
                return true;
            }
            let ours =
                held_leg.and_then(|l| l.dialogs.by_tags(&o.our_tag, &o.peer_tag).map(|d| (l, d)));
            let txn = ours.and_then(|(l, d)| sent_txn(l, d));
            txn.is_some_and(|t| t.branch == round.branch || t.cseq > round.cseq)
        })
    })
}

/// `held` with every round `incoming` stands ahead on ([`round_ahead`]) taken
/// from `incoming`, and nothing else: per such dialog, the INVITE it last sent
/// with the 2xx it took and that 2xx's ACK, the offer it owes an answer to,
/// its own sequence at the higher of the two; on the originator's dialog the
/// re-INVITE 2xx it awaits an ACK for and the higher remote sequence; that 2xx's ladder timers; and the session state of both legs
/// when `incoming` opened at least as many offer/answer exchanges on each of
/// them, so no exchange count is lowered (RFC 3264 §8). Every other fact is
/// `held`'s, its own requests and its own rounds on other dialogs included.
pub fn merge_rounds(mut held: Call, incoming: &Call) -> Call {
    let mut ahead = rounds_ended(incoming, &held);
    for opened in rounds_opened(incoming, &held) {
        if !ahead.iter().any(|r| r.leg_id == opened.leg_id && r.dialog == opened.dialog) {
            ahead.push(opened);
        }
    }
    for round in ahead {
        held = merge_round(held, incoming, &round);
    }
    held
}

/// A dialog on which `incoming` stands ahead of `held` on a round.
struct RoundAhead {
    leg_id: String,
    dialog: DialogId,
    /// The CSeq the round's originator sent its INVITE with, while the round
    /// is unfinished; `None` for a round `incoming` holds ACKed, whose 2xx
    /// awaits nothing.
    inbound_cseq: Option<i64>,
}

/// A round a dialog holds unfinished: its transaction, and where it stands.
struct Unfinished<'a> {
    branch: &'a str,
    /// The CSeq the round's INVITE went out with.
    cseq: i64,
    /// The CSeq the round's originator sent its INVITE with.
    inbound_cseq: i64,
    stage: Stage,
}

enum Stage {
    /// The relay at `outbound_cseq` awaits its final.
    Open { outbound_cseq: i64 },
    /// The 2xx is taken; the originator's ACK is awaited.
    Answered,
}

/// The client transaction of the INVITE a dialog of `leg` last sent: its
/// branch and the CSeq it went out with. `None` where the record holds none.
struct SentTxn<'a> {
    branch: &'a str,
    cseq: i64,
}

fn sent_txn<'a>(leg: &'a Leg, dialog: &'a Dialog) -> Option<SentTxn<'a>> {
    let record = dialog.sent_invite.as_ref()?;
    let sent = invite_image(leg, record)?;
    Some(SentTxn { branch: sent.branch.as_str(), cseq: record.outbound_cseq })
}

fn unfinished<'a>(leg: &'a Leg, dialog: &'a Dialog) -> Option<Unfinished<'a>> {
    let txn = sent_txn(leg, dialog)?;
    let (inbound_cseq, stage) = match invite_relays(dialog).next() {
        Some(relay) => (relay.inbound_cseq, Stage::Open { outbound_cseq: relay.outbound_cseq }),
        None => (awaited_ack_cseq(dialog)?, Stage::Answered),
    };
    Some(Unfinished { branch: txn.branch, cseq: txn.cseq, inbound_cseq, stage })
}

/// The relay of the INVITE `dialog` last sent, while it awaits its final.
fn invite_relays(dialog: &Dialog) -> impl Iterator<Item = &RequestRecord> {
    super::invite_records::invite_relay(dialog).into_iter()
}

/// Whether `incoming`, the same dialog of `leg` in the other copy, ended
/// `round`.
fn ended(leg: &Leg, incoming: &Dialog, round: &Unfinished<'_>) -> bool {
    let txn = sent_txn(leg, incoming);
    let same = txn.as_ref().is_some_and(|t| t.branch == round.branch);
    let later = txn.as_ref().is_some_and(|t| t.branch != round.branch && t.cseq > round.cseq);
    match round.stage {
        Stage::Open { outbound_cseq } => {
            let relayed = invite_relays(incoming).any(|p| p.outbound_cseq == outbound_cseq);
            let refused = txn.is_none() && incoming.our_cseq >= outbound_cseq;
            !relayed && (same || later || refused)
        }
        Stage::Answered => {
            let awaited = same && awaited_ack_cseq(incoming) == Some(round.inbound_cseq);
            let acked = same && ack_branch(incoming).is_some();
            !awaited && (acked || later)
        }
    }
}

/// Whether `ours`, the same dialog of `leg` in the other copy, reached the
/// round on the client transaction `branch` sent at `cseq`.
fn reached(leg: &Leg, ours: &Dialog, branch: &str, cseq: i64) -> bool {
    match sent_txn(leg, ours) {
        Some(t) => t.branch == branch || t.cseq > cseq,
        None => ours.our_cseq >= cseq,
    }
}

fn rounds_ended(incoming: &Call, held: &Call) -> Vec<RoundAhead> {
    let mut out = Vec::new();
    for held_leg in every_leg(held) {
        let Some(incoming_leg) = find_leg(incoming, &held_leg.leg_id) else { continue };
        for d in held_leg.dialogs.iter() {
            let Some(round) = unfinished(held_leg, d) else { continue };
            let Some(theirs) = incoming_leg.dialogs.by_tags(&d.our_tag, &d.peer_tag) else {
                continue;
            };
            if ended(incoming_leg, theirs, &round) {
                out.push(RoundAhead {
                    leg_id: held_leg.leg_id.clone(),
                    dialog: d.id(),
                    inbound_cseq: Some(round.inbound_cseq),
                });
            }
        }
    }
    out
}

fn rounds_opened(incoming: &Call, held: &Call) -> Vec<RoundAhead> {
    let mut out = Vec::new();
    for incoming_leg in every_leg(incoming) {
        let Some(held_leg) = find_leg(held, &incoming_leg.leg_id) else { continue };
        for d in incoming_leg.dialogs.iter() {
            let Some(txn) = sent_txn(incoming_leg, d) else { continue };
            let Some(ours) = held_leg.dialogs.by_tags(&d.our_tag, &d.peer_tag) else {
                continue;
            };
            if !reached(held_leg, ours, txn.branch, txn.cseq) {
                out.push(RoundAhead {
                    leg_id: incoming_leg.leg_id.clone(),
                    dialog: d.id(),
                    inbound_cseq: unfinished(incoming_leg, d).map(|r| r.inbound_cseq),
                });
            }
        }
    }
    out
}

fn dialog_of<'a>(call: &'a Call, leg_id: &str, id: &DialogId) -> Option<&'a Dialog> {
    find_leg(call, leg_id)?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
}

fn merge_round(held: Call, incoming: &Call, round: &RoundAhead) -> Call {
    let Some(theirs) = dialog_of(incoming, &round.leg_id, &round.dialog).cloned() else {
        return held;
    };
    let originator = relay_peer_dialog(&held, &round.leg_id, Some(&round.dialog))
        .map(|(leg, d)| (leg.leg_id.clone(), d.id()));
    let mut held = update_leg(held, &round.leg_id, |leg| {
        if let Some(d) = leg.dialogs.by_tags_mut(&round.dialog.our_tag, &round.dialog.peer_tag) {
            take_round(d, &theirs);
        }
    });
    let Some((o_leg, o_id)) = originator else {
        return take_sessions(held, incoming, &[&round.leg_id]);
    };
    let theirs_o = dialog_of(incoming, &o_leg, &o_id).cloned();
    let call_answer = (o_leg == held.a_leg.leg_id).then_some(i64::from(held.a_leg_invite.cseq));
    let is_call_answer = |r: &RequestRecord| Some(r.inbound_cseq) == call_answer;
    held = update_leg(held, &o_leg, |leg| {
        let (Some(d), Some(t)) = (leg.dialogs.by_tags_mut(&o_id.our_tag, &o_id.peer_tag), theirs_o)
        else {
            return;
        };
        // The other copy's answer to the round; the call's own answer is the
        // record's, whatever the round.
        let ours = d.answered_invite.take().filter(|r| is_call_answer(r));
        d.answered_invite = t.answered_invite.filter(|r| !is_call_answer(r)).or(ours);
        d.peer_cseq = d.peer_cseq.max(t.peer_cseq);
    });
    held = take_sessions(held, incoming, &[&round.leg_id, &o_leg]);
    match round.inbound_cseq {
        Some(from_cseq) => take_ladders(held, incoming, &o_leg, from_cseq),
        None => held,
    }
}

/// The INVITE round facts of the target dialog, from `theirs`: the INVITE it
/// last sent with the 2xx and ACK of it, and the offer it owes an answer to;
/// its own non-INVITE requests and the higher of the two sequences are kept.
fn take_round(d: &mut Dialog, theirs: &Dialog) {
    d.sent_invite = theirs.sent_invite.clone();
    d.owed_answer = theirs.owed_answer.clone();
    d.our_cseq = d.our_cseq.max(theirs.our_cseq);
}

/// The session state of the round's `legs`, from `incoming`, when it opened
/// at least as many offer/answer exchanges as `held` on each of them: it then
/// saw every exchange `held` saw on them, the round's included, and the
/// descriptions this stack last sent there. Otherwise `held` served an
/// exchange of its own the other copy never saw, and keeps its sessions: an
/// exchange count is never lowered, nor a description sent replaced by an
/// earlier one (RFC 3264 §8).
fn take_sessions(held: Call, incoming: &Call, legs: &[&str]) -> Call {
    let opened =
        |call: &Call, leg: &str| find_leg(call, leg).map(|l| l.sdp_session.exchanges_opened);
    let saw_every_exchange = legs.iter().all(|leg| opened(incoming, leg) >= opened(&held, leg));
    if !saw_every_exchange {
        return held;
    }
    legs.iter().fold(held, |held, leg| {
        let Some(session) = find_leg(incoming, leg).map(|l| l.sdp_session.clone()) else {
            return held;
        };
        update_leg(held, leg, |l| l.sdp_session = session)
    })
}

/// The ladders of the re-INVITE 2xx the originator's leg awaits ACKs for, from
/// the round's on: `held`'s go, `incoming`'s come in.
fn take_ladders(mut held: Call, incoming: &Call, leg_id: &str, from_cseq: i64) -> Call {
    let ladder = |t: &TimerEntry| {
        matches!(t.key.obligation_of(), Some(Obligation::AckOf2xx { leg, cseq, .. })
            if leg == leg_id && *cseq >= from_cseq)
    };
    held.timers.retain(|t| !ladder(t));
    held.timers.extend(incoming.timers.iter().filter(|t| ladder(t)).cloned());
    held
}
