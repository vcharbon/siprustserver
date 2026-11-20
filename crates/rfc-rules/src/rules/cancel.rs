//! RFC 3261 §9 — the CANCEL family: what a UAC must have heard before it
//! cancels and what its CANCEL must carry (§9.1), and how the UAS must answer
//! a cancelled INVITE (§9.2).
//!
//! **Every rule pairs a CANCEL with its INVITE by TRANSACTION**: the call, the
//! top-Via branch the CANCEL reuses (§9.1, §17.2.3) and the endpoint judged.
//! Call-ID and CSeq number do not name a transaction: an INVITE re-sent on both
//! under a new branch is a new one, whatever `cseq-in-dialog-order` makes of
//! the number it repeats. The §9.2
//! rule reads the transaction as its UAS took it and [`NoCancelAfterFinal`] as
//! its UAC drove it, both here; the two other §9.1 rules read the walk every
//! branch-pairing family shares, in `super::branch`.
//!
//! **Repeats are not fresh events** in any of them: a message the adapter
//! marked [`Msg::repeat`] opens no occasion, since retransmitting until
//! answered is required behaviour, never a second act.
//!
//! **A message with no top-Via branch keys no occasion.** The branch is how a
//! CANCEL names its INVITE; without one the pair cannot be formed, and the
//! rule states nothing rather than guessing at a pairing.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Msg, WireView};

use super::branch::BranchReading;
use super::sides::Sides;
use super::Obligation;

mod keys;
mod late;
use keys::TxnKey;
pub use late::NoCancelAfterFinal;

crate::registry::rules! {
    /// RFC 3261 §9.2: a UAS that has taken a CANCEL for an INVITE answers that INVITE 487, never
    /// 2xx.
    No200AfterCancel = "no-200-after-cancel";
    /// RFC 3261 §9.1: a CANCEL carries the same Route values as the INVITE it cancels — the two
    /// take the same path. Charges the CANCEL's sender.
    CancelRouteEchoesInvite = "cancel-route-echoes-invite";
    /// RFC 3261 §9.1: a UAC CANCELs only once it has taken a provisional. Charges the UAC.
    CancelAfter1xx = "cancel-after-1xx";
    /// RFC 3261 §9.1: a UAC CANCELs only a client transaction still in flight — after its final
    /// (§17.1.1.2) a CANCEL reaches no transaction (481, §9.2). Charges the CANCEL's sender.
    NoCancelAfterFinal = "no-cancel-after-final";
}

/// **RFC 3261 §9.2 — no 2xx after a CANCEL.**
///
/// **The occasion is the cancelled TRANSACTION, as ONE endpoint saw it**: the
/// endpoint took the INVITE, then took a CANCEL on that INVITE's top-Via
/// branch. Both facts are on the wire, so every occasion is DECIDED — this rule
/// has no absence to wait out and no undecidable verdict, and a closed
/// observation reads exactly like an open one.
///
/// **Three keys decide it: the call, the branch, and DIRECTION.** A CANCEL
/// matches the server transaction whose INVITE carried its top-Via branch
/// (§9.2, §17.2.3), and the UAS's 2xx echoes that Via; an INVITE re-offered on
/// the same Call-ID and CSeq number under a new branch is a transaction no
/// CANCEL touched. The call keeps two calls' branches apart on a view carrying
/// many, and direction measures an endpoint only against the INVITEs it TOOK
/// and the responses it EMITTED.
///
/// **A 2xx emitted BEFORE the CANCEL arrived is a crossing, not a violation:**
/// the two messages passed each other in flight and the UAS answered an INVITE
/// it had no CANCEL for. The crossing still costs an occasion — a report says
/// how rare the violation is among the transactions that could have produced
/// it. A 2xx the adapter marked [`Msg::repeat`] is the crossing answer again,
/// never a second one (§13.3.1.4 obliges the repeat).
///
/// **An emitter that forwarded a 2xx it had already been sent is marked
/// `relayed`** — the far endpoint that originated the answer is charged
/// separately, and consumer policy weighs the hop's finding.
///
/// Charges the UAS side of the cancelled transaction: the endpoint that took
/// the INVITE and its CANCEL and then answered 2xx anyway. Answering 487 — or
/// answering nothing this vantage carried — discharges it.
pub struct No200AfterCancel;

impl Obligation for No200AfterCancel {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (&TxnKey { emitter, .. }, txn) in &seen.txns {
            // Only the UAS side of a transaction this vantage actually saw
            // opened is judged: an endpoint measured against an INVITE nobody
            // observed it take is measured against nothing.
            let Some(cancel) = txn.cancel.as_ref().filter(|_| txn.took_invite_us.is_some()) else {
                continue;
            };
            let cseq = wire.msgs[cancel.msg].cseq;
            // The FIRST answer that followed the CANCEL is the offence; a
            // later one repeats the same broken answer under another fork.
            let Some(answer) = txn.answers.iter().find(|a| cancel.ts_us < a.ts_us) else {
                out.push(Finding {
                    rule: RuleId::No200AfterCancel,
                    emitter: emitter.to_string(),
                    taker: cancel.src.to_string(),
                    cseq,
                    relayed: false,
                    anchor: cancel.msg,
                    decision: Decision::Compliant,
                });
                continue;
            };
            out.push(Finding {
                rule: RuleId::No200AfterCancel,
                emitter: emitter.to_string(),
                taker: answer.taker.to_string(),
                cseq,
                relayed: answer.relayed,
                anchor: answer.msg,
                decision: Decision::Violated(Evidence::Cancelled {
                    cancel_msg: cancel.msg,
                    cancel_hop: cancel.hop,
                    cancel_ts_us: cancel.ts_us,
                    response_msg: answer.msg,
                    response_hop: answer.hop,
                    response_ts_us: answer.ts_us,
                    status: answer.status,
                    gap_us: answer.ts_us - cancel.ts_us,
                }),
            });
        }
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// What one view's messages say about the cancelled transactions on it.
///
/// Collected in one pass and judged in a second, because the two facts a
/// verdict needs arrive in either order: a crossing puts the answer first, and
/// it is as much a race as the violation is.
#[derive(Debug, Default)]
struct Reading<'a> {
    /// The INVITE transaction each endpoint saw, keyed by the branch a CANCEL
    /// reuses.
    txns: BTreeMap<TxnKey<'a>, Txn<'a>>,
    /// (transaction, To tag) → this dialog's final was already seen.
    answered: BTreeSet<(TxnKey<'a>, &'a str)>,
    /// (call, CSeq number, To tag) → every time a 2xx so named crossed ANY
    /// vantage of the view. An emitter that emits it after an earlier sighting
    /// later than the INVITE it took is relaying, whatever address the relaying
    /// box wears on its far interface; a sighting before it took that INVITE
    /// answered another transaction.
    sightings: BTreeMap<(&'a str, u32, &'a str), Vec<u64>>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let sides = Sides::of(msgs);
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(&sides, mi, msg);
        }
        seen
    }

    fn absorb(&mut self, sides: &Sides<'a>, mi: usize, msg: &'a Msg) {
        if msg.repeat {
            return;
        }
        let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else { return };
        if msg.is_request("INVITE") {
            self.txns
                .entry(TxnKey::taken_by(msg, branch))
                .or_default()
                .took_invite_us
                .get_or_insert(msg.at_us);
        } else if msg.is_request("CANCEL") {
            // A CANCEL rides the top-Via branch of the INVITE it cancels; the
            // earliest arrival is when the UAS took it.
            self.txns.entry(TxnKey::taken_by(msg, branch)).or_default().cancel.get_or_insert(
                Cancel { msg: mi, hop: msg.hop, ts_us: msg.at_us, src: msg.src.as_str() },
            );
        } else if let Some(status) = msg.status() {
            if !(200..300).contains(&status) || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            let txn = TxnKey::emitted_by(sides, msg, branch);
            let to_tag = msg.to_tag.as_deref().unwrap_or_default();
            let sightings =
                self.sightings.entry((msg.call_id.as_str(), msg.cseq, to_tag)).or_default();
            let took_us = self.txns.get(&txn).and_then(|t| t.took_invite_us);
            let relayed =
                sightings.iter().any(|&at| at < msg.at_us && took_us.is_none_or(|took| took <= at));
            sightings.push(msg.at_us);
            if !self.answered.insert((txn, to_tag)) {
                return; // a second 2xx of a dialog already answered
            }
            self.txns.entry(txn).or_default().answers.push(Answer {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
                taker: msg.dst.as_str(),
                relayed,
            });
        }
    }
}

/// What one endpoint saw of one transaction: whether it took the INVITE, the
/// CANCEL it took, and the 2xxs it emitted.
#[derive(Debug, Default)]
struct Txn<'a> {
    /// When this endpoint received the INVITE: it is the transaction's UAS
    /// side.
    took_invite_us: Option<u64>,
    /// The first CANCEL it took for that INVITE.
    cancel: Option<Cancel<'a>>,
    /// Every distinct 2xx it emitted to that INVITE, in observation order.
    answers: Vec<Answer<'a>>,
}

/// The CANCEL an endpoint took, and who sent it.
#[derive(Debug)]
struct Cancel<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    src: &'a str,
}

/// One 2xx an endpoint emitted to an INVITE.
#[derive(Debug)]
struct Answer<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    taker: &'a str,
    /// The emitter had already been sent this same 2xx: it forwarded an answer
    /// the far endpoint originated.
    relayed: bool,
}

/// **RFC 3261 §9.1 — a CANCEL carries the INVITE's Route values.**
///
/// A CANCEL takes the same path as the INVITE it cancels, so it carries that
/// INVITE's Route set unchanged; a divergent Route set sends the cancellation
/// down a path where no server transaction is waiting for it.
///
/// **The occasion is one CANCEL an endpoint SENT on a branch it had already
/// sent the INVITE on** — that pairing (§9.1 reuses the INVITE's top-Via
/// branch) is what makes the two comparable. A CANCEL whose INVITE this vantage
/// never saw the same emitter send is no occasion: there is nothing to echo.
///
/// **The comparison is the Route ROWS, in order.** Two messages carry the same
/// Route set when their Route rows read equal one for one — a re-folded or
/// reordered set is a different path statement, and §9.1 asks for a copy.
///
/// **An unreadable header block is `Undecidable`, never clean.** The rows are
/// read off the messages' bytes; a vantage that carried no header block for
/// either message settles nothing about what they said.
///
/// Charges the CANCEL's sender.
pub struct CancelRouteEchoesInvite;

impl Obligation for CancelRouteEchoesInvite {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            // No INVITE from this emitter on this branch: nothing to echo, so
            // no occasion at all.
            let Some(invite) = branch.first_sent("INVITE") else { continue };
            for cancel in branch.sent_of("CANCEL") {
                let finding = |decision| Finding {
                    rule: RuleId::CancelRouteEchoesInvite,
                    emitter: key.emitter.to_string(),
                    taker: cancel.taker.to_string(),
                    cseq: cancel.cseq,
                    relayed: false,
                    anchor: cancel.msg,
                    decision,
                };
                let (Some(cancel_head), Some(invite_head)) =
                    (wire.msgs[cancel.msg].head.as_deref(), wire.msgs[invite.msg].head.as_deref())
                else {
                    out.push(finding(Decision::Undecidable("no header block at this vantage")));
                    continue;
                };
                let cancel_routes = sniff::header_values(cancel_head, "route");
                let invite_routes = sniff::header_values(invite_head, "route");
                if cancel_routes == invite_routes {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
                out.push(finding(Decision::Violated(Evidence::CancelRouteDiverged {
                    cancel_msg: cancel.msg,
                    cancel_hop: cancel.hop,
                    cancel_ts_us: cancel.ts_us,
                    invite_msg: invite.msg,
                    invite_hop: invite.hop,
                    invite_ts_us: invite.ts_us,
                    cancel_routes,
                    invite_routes,
                    branch: key.branch.to_string(),
                })));
            }
        }
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §9.1 — a UAC waits for a provisional before it CANCELs.**
///
/// A CANCEL is matchable only once the UAS has answered something: a UAS that
/// has not responded may not have built the INVITE server transaction yet, so
/// the early CANCEL draws a 481 while Timer-A retransmits keep ringing a call
/// nobody can cancel any more.
///
/// **The occasion is one CANCEL an endpoint SENT on a branch it had already
/// sent the INVITE on** (§9.1 reuses that branch). Only a 1xx the emitter TOOK
/// on the branch BEFORE the CANCEL in view order discharges it; no elapsed
/// time does.
///
/// **The view carries the whole stream, so a 1xx is never lost to a slice.**
/// The rule reads one vantage's entire observation, not a per-dialog slice of
/// it, so a provisional that confirmed the dialog and the CANCEL that followed
/// it are always read together: order in the view is the whole of the "before"
/// test.
///
/// Charges the CANCEL's sender.
pub struct CancelAfter1xx;

impl Obligation for CancelAfter1xx {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            // An endpoint measured against an INVITE this vantage never saw it
            // send is measured against nothing: no INVITE, no occasion.
            let Some(invite) = branch.first_sent("INVITE") else { continue };
            for cancel in branch.sent_of("CANCEL") {
                let finding = |decision| Finding {
                    rule: RuleId::CancelAfter1xx,
                    emitter: key.emitter.to_string(),
                    taker: cancel.taker.to_string(),
                    cseq: cancel.cseq,
                    relayed: false,
                    anchor: cancel.msg,
                    decision,
                };
                // The §9.1 wait, honoured: a provisional was in hand.
                if branch.first_1xx.is_some_and(|at| at < cancel.msg) {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
                let since_invite_us = cancel.ts_us.saturating_sub(invite.ts_us);
                out.push(finding(Decision::Violated(Evidence::EagerCancel {
                    cancel_msg: cancel.msg,
                    cancel_hop: cancel.hop,
                    cancel_ts_us: cancel.ts_us,
                    invite_msg: invite.msg,
                    invite_hop: invite.hop,
                    invite_ts_us: invite.ts_us,
                    since_invite_us,
                    branch: key.branch.to_string(),
                })));
            }
        }
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

#[cfg(test)]
mod tests;
