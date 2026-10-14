//! The re-INVITE rounds two copies of one call share, and how one copy's view
//! of a round's end lands in the other (ADR-0031, "a re-INVITE round is a
//! third chain").
//!
//! A round is the INVITE client transaction one dialog relays toward its peer,
//! named by the leg, the dialog's two tags and the transaction's branch. It
//! goes open (its relay awaits a final) → answered (a 2xx taken, the
//! originator's ACK awaited) → ACKed, or ends on a non-2xx, and never goes
//! back. Its final reaches the node holding the client transaction, so a
//! round's end another copy saw is progress a record holding the round
//! unfinished cannot make itself. The rules read the rounds that record holds;
//! a round only the other copy holds is no progress under them.

use crate::model::{Call, Dialog, DialogId, Leg, Obligation, PendingRequest, TimerEntry};

use super::leg::find_leg;
use super::lens::update_leg;
use super::peering::relay_peer_dialog;

/// Does `incoming` stand further along a round `held` holds unfinished, on
/// any dialog? Further along an open round: `incoming`'s same dialog no longer
/// holds the relay at the round's CSeq and its own sequence is at or past it
/// — it saw the round end, answered or refused, and possibly a next round
/// begin. Further along an answered round: `incoming` no longer awaits that
/// 2xx's ACK, and either the same transaction's ACK left or a next round's
/// transaction stands on the dialog at or past `held`'s sequence.
pub fn round_further(incoming: &Call, held: &Call) -> bool {
    !rounds_ended(incoming, held).is_empty()
}

/// `held` with every round `incoming` stands further along taken from
/// `incoming`, and nothing else: per such dialog, its INVITE relays, its
/// INVITE client transaction and the ACK state of that transaction's 2xx, the
/// offer it owes an answer to, its own sequence at the higher of the two; on
/// the originator's dialog the 2xx it awaits an ACK for and the higher remote
/// sequence; that 2xx's ladder timers; and the session state of both legs
/// when `incoming` opened at least as many offer/answer exchanges on each of
/// them, so no exchange count is lowered (RFC 3264 §8). Every other fact is
/// `held`'s, its own requests on either dialog included.
pub fn merge_rounds(mut held: Call, incoming: &Call) -> Call {
    for ended in rounds_ended(incoming, &held) {
        held = merge_round(held, incoming, &ended);
    }
    held
}

/// Where a round `held` holds unfinished ended in `incoming`.
struct RoundEnd {
    leg_id: String,
    dialog: DialogId,
    /// The CSeq the round's originator sent its INVITE with.
    inbound_cseq: i64,
}

/// What `held` holds of a dialog's round, while unfinished.
enum Unfinished<'a> {
    Open { outbound_cseq: i64, inbound_cseq: i64 },
    Answered { branch: &'a str, inbound_cseq: i64, our_cseq: i64 },
}

fn unfinished(dialog: &Dialog) -> Option<Unfinished<'_>> {
    let txn = dialog.pending_invite_txn.as_ref()?;
    match invite_relays(dialog).next() {
        Some(relay) => Some(Unfinished::Open {
            outbound_cseq: relay.outbound_cseq,
            inbound_cseq: relay.inbound_cseq,
        }),
        None => dialog.awaited_ack_cseq.map(|inbound_cseq| Unfinished::Answered {
            branch: txn.branch.as_str(),
            inbound_cseq,
            our_cseq: dialog.our_cseq,
        }),
    }
}

fn invite_relays(dialog: &Dialog) -> impl Iterator<Item = &PendingRequest> {
    dialog.inbound_pending_requests.iter().filter(|p| p.method.eq_ignore_ascii_case("INVITE"))
}

/// Whether `incoming`, the same dialog in the other copy, ended `round`.
fn ended(incoming: &Dialog, round: &Unfinished<'_>) -> bool {
    match round {
        Unfinished::Open { outbound_cseq, .. } => {
            incoming.our_cseq >= *outbound_cseq
                && !incoming
                    .inbound_pending_requests
                    .iter()
                    .any(|p| p.outbound_cseq == *outbound_cseq)
        }
        Unfinished::Answered { branch, inbound_cseq, our_cseq } => {
            let txn = incoming.pending_invite_txn.as_ref();
            let same = txn.is_some_and(|t| t.branch == *branch);
            let awaited = same && incoming.awaited_ack_cseq == Some(*inbound_cseq);
            let acked = same && incoming.ack_branch.is_some();
            let next_round =
                txn.is_some_and(|t| t.branch != *branch) && incoming.our_cseq >= *our_cseq;
            !awaited && (acked || next_round)
        }
    }
}

fn rounds_ended(incoming: &Call, held: &Call) -> Vec<RoundEnd> {
    let mut out = Vec::new();
    for held_leg in legs(held) {
        let Some(incoming_leg) = find_leg(incoming, &held_leg.leg_id) else { continue };
        for d in held_leg.dialogs.iter() {
            let Some(round) = unfinished(d) else { continue };
            let Some(theirs) = incoming_leg.dialogs.by_tags(&d.our_tag, &d.peer_tag) else {
                continue;
            };
            if ended(theirs, &round) {
                let inbound_cseq = match round {
                    Unfinished::Open { inbound_cseq, .. }
                    | Unfinished::Answered { inbound_cseq, .. } => inbound_cseq,
                };
                out.push(RoundEnd {
                    leg_id: held_leg.leg_id.clone(),
                    dialog: d.id(),
                    inbound_cseq,
                });
            }
        }
    }
    out
}

fn legs(call: &Call) -> impl Iterator<Item = &Leg> {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter())
}

fn dialog_of<'a>(call: &'a Call, leg_id: &str, id: &DialogId) -> Option<&'a Dialog> {
    find_leg(call, leg_id)?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
}

fn merge_round(held: Call, incoming: &Call, round: &RoundEnd) -> Call {
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
    held = update_leg(held, &o_leg, |leg| {
        let (Some(d), Some(t)) = (leg.dialogs.by_tags_mut(&o_id.our_tag, &o_id.peer_tag), theirs_o)
        else {
            return;
        };
        d.pending_reinvite_2xx = t.pending_reinvite_2xx;
        d.peer_cseq = d.peer_cseq.max(t.peer_cseq);
    });
    held = take_sessions(held, incoming, &[&round.leg_id, &o_leg]);
    take_ladders(held, incoming, &o_leg, round.inbound_cseq)
}

/// The INVITE round facts of the target dialog, from `theirs`; its own
/// non-INVITE relays and the higher of the two sequences are kept.
fn take_round(d: &mut Dialog, theirs: &Dialog) {
    d.inbound_pending_requests.retain(|p| !p.method.eq_ignore_ascii_case("INVITE"));
    d.inbound_pending_requests.extend(invite_relays(theirs).cloned());
    d.pending_invite_txn = theirs.pending_invite_txn.clone();
    d.awaited_ack_cseq = theirs.awaited_ack_cseq;
    d.ack_branch = theirs.ack_branch.clone();
    d.emitted_ack = theirs.emitted_ack.clone();
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
