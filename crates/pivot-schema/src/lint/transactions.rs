//! INVITE transactions as a leg's STATE holds them, read from step order on
//! one run of the flow: which INVITE a final lands on and which final an ACK
//! discharges.
//!
//! Every INVITE request on a leg opens a transaction in the direction it runs.
//! A final to an INVITE lands on the newest transaction running the other way
//! — a response travels opposite to its request, and a leg runs one CSeq space
//! per direction (RFC 3261 §12.2) — and REPLACES the final it held: a second
//! final on one INVITE (a fork's 2xx, a re-emission) is its own final owed its
//! own ACK (§13.2.2.4). The newest transaction takes it whatever it holds, so
//! a 2xx re-emitted after a newer INVITE of its direction was answered reads
//! as that INVITE's: an authored shape, since the cut folds a repeat onto the
//! step it repeats. A final arriving before any INVITE of its direction opens
//! the transaction itself: the INVITE ran before the flow starts.
//!
//! An ACK discharges the newest transaction running its own way that holds a
//! final and no ACK yet. A non-2xx final consumes an ACK the same way
//! (§17.1.1.3), so an ACK sent while an older 2xx still waits — the 491 round
//! of a re-INVITE sent over an un-ACKed 2xx (§14.1) — discharges the newer
//! transaction and leaves the older one owed. The reading is by position: the
//! captured `cseq` is never read, and a leg whose ACKs run in the other order
//! is read the other way round.
//!
//! A run is what `reach` says it is: the steps replayed before a place are the
//! ones every run through that place has already taken, so a branch reads only
//! the transactions its own run opened.

use crate::flow::{Op, Step};
use crate::lint::{reach, Place, Reach};

/// One INVITE transaction a leg holds.
struct InviteTransaction<'a> {
    /// The direction the INVITE ran: the op of its request step.
    op: Op,
    /// The INVITE step; absent where the run starts after it.
    invite: Option<&'a Step>,
    /// The final the leg holds for it; absent while open.
    final_: Option<&'a Step>,
    /// Whether an ACK discharged that final.
    acked: bool,
}

/// What names one transaction across two reads of the same run: the direction
/// it runs and its INVITE step, or no step where the run starts after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TransactionId<'a> {
    pub op: Op,
    pub invite: Option<&'a str>,
}

/// What one ACK discharged: the transaction, and the final it held then.
pub(super) struct Discharge<'a> {
    pub transaction: TransactionId<'a>,
    pub final_: &'a Step,
}

fn other(op: Op) -> Op {
    match op {
        Op::Send => Op::Expect,
        Op::Expect => Op::Send,
    }
}

fn is_invite(step: &Step) -> bool {
    step.msg.status.is_none()
        && step.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

fn is_invite_final(step: &Step) -> bool {
    step.msg.status.is_some_and(|s| s >= 200)
        && step.msg.cseq_method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("INVITE"))
}

fn is_ack(step: &Step) -> bool {
    step.msg.status.is_none()
        && step.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("ACK"))
}

impl<'a> InviteTransaction<'a> {
    fn id(&self) -> TransactionId<'a> {
        TransactionId { op: self.op, invite: self.invite.map(|s| s.id.as_str()) }
    }
}

/// The transaction a final landing now lands on, opening one where the leg
/// holds none running the other way.
fn land<'a, 'h>(
    held: &'h mut Vec<InviteTransaction<'a>>,
    final_: &Step,
) -> &'h mut InviteTransaction<'a> {
    let op = other(final_.op);
    let at = match held.iter().rposition(|t| t.op == op) {
        Some(at) => at,
        None => {
            held.push(InviteTransaction { op, invite: None, final_: None, acked: false });
            held.len() - 1
        }
    };
    &mut held[at]
}

/// The leg's transactions as they stand when `place` runs: replayed over the
/// steps of `leg` that `place`'s own run has already taken, `place` excluded.
fn before<'a>(all: &[(Place, &'a Step)], leg: &str, place: Place) -> Vec<InviteTransaction<'a>> {
    let mut held: Vec<InviteTransaction<'a>> = Vec::new();
    for (_, step) in all
        .iter()
        .filter(|(other_place, other)| other.leg == leg && reach(*other_place, place) == Reach::Ok)
    {
        if is_invite(step) {
            held.push(InviteTransaction {
                op: step.op,
                invite: Some(step),
                final_: None,
                acked: false,
            });
        } else if is_invite_final(step) {
            let landing = land(&mut held, step);
            landing.final_ = Some(step);
            landing.acked = false;
        } else if is_ack(step) {
            if let Some(awaiting) =
                held.iter_mut().rev().find(|t| t.op == step.op && t.final_.is_some() && !t.acked)
            {
                awaiting.acked = true;
            }
        }
    }
    held
}

/// The transaction the INVITE final at `place` lands on.
pub(super) fn landing_of<'a>(
    all: &[(Place, &'a Step)],
    final_: &Step,
    place: Place,
) -> TransactionId<'a> {
    let mut held = before(all, &final_.leg, place);
    land(&mut held, final_).id()
}

/// What the ACK at `place` discharges; nothing where no transaction running its
/// way holds an un-ACKed final.
pub(super) fn discharged_by<'a>(
    all: &[(Place, &'a Step)],
    ack: &Step,
    place: Place,
) -> Option<Discharge<'a>> {
    before(all, &ack.leg, place)
        .into_iter()
        .rev()
        .find(|t| t.op == ack.op && t.final_.is_some() && !t.acked)
        .map(|t| Discharge { transaction: t.id(), final_: t.final_.expect("a final it holds") })
}
