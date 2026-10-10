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
use crate::wire::{Kind, Msg, WireView};

use super::branch::BranchReading;
use super::Obligation;

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
    fn id(&self) -> RuleId {
        RuleId::No200AfterCancel
    }

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
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
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
            let txn = TxnKey::emitted_by(msg, branch);
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

/// One INVITE transaction as ONE endpoint saw it: the call it rides, the
/// top-Via branch that names it, and which endpoint is its UAS side. Spelled
/// out as a struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    /// The endpoint judged: the UAS that TOOK the INVITE and emits its answers.
    emitter: &'a str,
    call_id: &'a str,
    branch: &'a str,
}

impl<'a> TxnKey<'a> {
    /// The transaction `msg`'s RECIPIENT is the UAS side of — an INVITE or its
    /// CANCEL arriving on `branch`.
    fn taken_by(msg: &'a Msg, branch: &'a str) -> Self {
        TxnKey { emitter: msg.dst.as_str(), call_id: msg.call_id.as_str(), branch }
    }

    /// The transaction `msg`'s SENDER is the UAS side of — a response going out
    /// on `branch`.
    fn emitted_by(msg: &'a Msg, branch: &'a str) -> Self {
        TxnKey { emitter: msg.src.as_str(), call_id: msg.call_id.as_str(), branch }
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
    fn id(&self) -> RuleId {
        RuleId::CancelRouteEchoesInvite
    }

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

/// Acceptance floor for a pre-1xx CANCEL, microseconds: a CANCEL at least this
/// far after its branch's first INVITE is a grace-expiry send, not the
/// eager-CANCEL defect. Sits just below the transaction layer's
/// `sip_txn::timers::CANCEL_HOLD_GRACE` — a floor equal to the window would
/// false-fire on stamp granularity — and the two move together.
pub const CANCEL_GRACE_FLOOR_US: u64 = 900_000;

/// **RFC 3261 §9.1 — a UAC waits for a provisional before it CANCELs**, bounded
/// by an implementation's grace window (ADR-0028). Advisory by consumer policy.
///
/// A CANCEL is matchable only once the UAS has answered something: a UAS that
/// has not responded may not have built the INVITE server transaction yet, so
/// the early CANCEL draws a 481 while Timer-A retransmits keep ringing a call
/// nobody can cancel any more. A bounded hold is the sanctioned answer — a
/// callee that answers nothing must still hear the cancellation — so this rule
/// separates the two by timing rather than forbidding the pre-1xx send.
///
/// **The occasion is one CANCEL an endpoint SENT on a branch it had already
/// sent the INVITE on** (§9.1 reuses that branch). Two things discharge it: a
/// 1xx the emitter TOOK on the branch BEFORE the CANCEL in view order, or a
/// CANCEL that left at least [`CANCEL_GRACE_FLOOR_US`] after the emitter's
/// FIRST INVITE on the branch — the grace-expiry send toward a response-less
/// callee. Everything else is the eager CANCEL.
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
    fn id(&self) -> RuleId {
        RuleId::CancelAfter1xx
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            // An endpoint measured against an INVITE this vantage never saw it
            // send is measured against nothing: no INVITE, no grace window, no
            // occasion.
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
                if since_invite_us >= CANCEL_GRACE_FLOOR_US {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
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

/// **RFC 3261 §9.1 — no CANCEL once the final has landed.**
///
/// A CANCEL cancels a client transaction still in flight. Once a final response
/// has arrived the INVITE transaction is completed (§17.1.1.2) and the CANCEL
/// names no transaction the server still holds, so it draws a 481 (§9.2) and
/// changes nothing.
///
/// **The occasion is the INVITE transaction as its UAC drove it** — the call,
/// the top-Via branch the CANCEL reuses, and DIRECTION, the reading
/// [`No200AfterCancel`] uses from the other side. A final that ended an
/// earlier attempt on the same Call-ID and CSeq number completed a different
/// transaction and says nothing about this CANCEL.
///
/// **The emitter's own ACK is what decides it.** A final observed just before
/// the CANCEL may have crossed it in flight, and a capture point away from the
/// emitter can order the two wrongly; an ACK the EMITTER sent for that final
/// cannot have crossed anything. A non-2xx final's ACK rides the INVITE's
/// branch (§17.1.1.3). A 2xx's ACK is its own transaction in the dialog the
/// 2xx made (§13.2.2.4): the 2xx's CSeq number and To tag, on a branch the
/// emitter sent no INVITE on — an ACK on an INVITE's branch acknowledges that
/// INVITE's non-2xx.
/// So the rule charges only a CANCEL sent after the emitter had ACKed the
/// final, and every other ordering is compliant — under-reporting the racy half
/// rather than charging a crossing.
///
/// Charges the CANCEL's sender.
pub struct NoCancelAfterFinal;

impl Obligation for NoCancelAfterFinal {
    fn id(&self) -> RuleId {
        RuleId::NoCancelAfterFinal
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = UacReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, txn) in &seen.txns {
            for cancel in &txn.cancels {
                // An INVITE this vantage never saw the emitter send on the
                // branch ahead of the CANCEL is no occasion at all.
                let Some(invite) = txn.invites.iter().rev().find(|i| i.msg < cancel.msg) else {
                    continue;
                };
                let finding = |decision| Finding {
                    rule: RuleId::NoCancelAfterFinal,
                    emitter: key.emitter.to_string(),
                    taker: cancel.taker.to_string(),
                    cseq: wire.msgs[cancel.msg].cseq,
                    relayed: false,
                    anchor: cancel.msg,
                    decision,
                };
                out.push(finding(match completed(&seen, key, txn, invite, cancel) {
                    None => Decision::Compliant,
                    Some(evidence) => Decision::Violated(evidence),
                }));
            }
        }
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The proof that `cancel` left after its own transaction had completed: the
/// final the emitter took on the branch, and the ACK it sent for that final
/// before cancelling. `None` wherever either is missing or out of order, which
/// is every compliant CANCEL and every crossing this vantage cannot separate.
fn completed(
    seen: &UacReading<'_>,
    key: &UacKey<'_>,
    txn: &UacTxn<'_>,
    invite: &Emitted<'_>,
    cancel: &Emitted<'_>,
) -> Option<Evidence> {
    let fin = txn.finals.iter().find(|f| f.msg > invite.msg && f.msg < cancel.msg)?;
    let in_window = |a: &&Emitted<'_>| a.msg > fin.msg && a.msg < cancel.msg;
    let ack = if (200..300).contains(&fin.status) {
        let dialog = AckDialog {
            emitter: key.emitter,
            call_id: key.call_id,
            cseq: fin.cseq,
            to_tag: fin.to_tag,
        };
        seen.acks_2xx
            .get(&dialog)?
            .iter()
            .filter(|sent| !seen.sent_invite_on(key.emitter, key.call_id, sent.branch))
            .map(|sent| &sent.ack)
            .find(in_window)?
    } else {
        txn.acks.iter().find(in_window)?
    };
    Some(Evidence::LateCancel {
        cancel_msg: cancel.msg,
        cancel_hop: cancel.hop,
        cancel_ts_us: cancel.ts_us,
        invite_msg: invite.msg,
        invite_hop: invite.hop,
        invite_ts_us: invite.ts_us,
        final_msg: fin.msg,
        final_hop: fin.hop,
        final_ts_us: fin.ts_us,
        final_status: fin.status,
        ack_msg: ack.msg,
        ack_hop: ack.hop,
        ack_ts_us: ack.ts_us,
        since_final_us: cancel.ts_us.saturating_sub(fin.ts_us),
    })
}

/// One INVITE transaction as ONE endpoint DROVE it: the call it rides, the
/// top-Via branch that names it, and which endpoint is its UAC side. The mirror
/// of [`TxnKey`], which names the same transaction by its UAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct UacKey<'a> {
    /// The endpoint judged: the UAC that SENT the INVITE and its CANCEL.
    emitter: &'a str,
    call_id: &'a str,
    branch: &'a str,
}

impl<'a> UacKey<'a> {
    /// The transaction `msg`'s SENDER drives — a request going out on `branch`.
    fn sent_by(msg: &'a Msg, branch: &'a str) -> Self {
        UacKey { emitter: msg.src.as_str(), call_id: msg.call_id.as_str(), branch }
    }

    /// The transaction `msg`'s RECIPIENT drives — a response coming back on
    /// `branch`.
    fn taken_by(msg: &'a Msg, branch: &'a str) -> Self {
        UacKey { emitter: msg.dst.as_str(), call_id: msg.call_id.as_str(), branch }
    }
}

/// What one endpoint did on one branch as UAC, in view order: the INVITEs it
/// sent, the finals it took, and the ACKs and CANCELs it sent there.
#[derive(Debug, Default)]
struct UacTxn<'a> {
    invites: Vec<Emitted<'a>>,
    finals: Vec<Final<'a>>,
    acks: Vec<Emitted<'a>>,
    cancels: Vec<Emitted<'a>>,
}

/// One request an endpoint sent on the transaction, and where it went.
#[derive(Debug, Clone, Copy)]
struct Emitted<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    taker: &'a str,
}

/// The first final an endpoint took for its own INVITE.
#[derive(Debug)]
struct Final<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    /// The CSeq number and To tag a 2xx's ACK carries (§13.2.2.4).
    cseq: u32,
    to_tag: &'a str,
}

/// What one view's messages say about the transactions each endpoint drove as
/// UAC. Repeats are skipped: retransmitting until answered is required
/// behaviour, never a second act.
#[derive(Debug, Default)]
struct UacReading<'a> {
    txns: BTreeMap<UacKey<'a>, UacTxn<'a>>,
    /// (emitter, call, CSeq number, To tag) → every ACK the emitter sent in that
    /// dialog, with its branch, in view order: a 2xx's ACK rides a branch of
    /// its own, so the dialog is what ties it to the final.
    acks_2xx: BTreeMap<AckDialog<'a>, Vec<SentAck<'a>>>,
}

/// The dialog a 2xx's ACK names (§13.2.2.4): its sender, the call, the
/// INVITE's CSeq number and the 2xx's To tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AckDialog<'a> {
    emitter: &'a str,
    call_id: &'a str,
    cseq: u32,
    to_tag: &'a str,
}

/// An ACK an endpoint sent, and the top-Via branch it rode.
#[derive(Debug)]
struct SentAck<'a> {
    ack: Emitted<'a>,
    branch: &'a str,
}

impl<'a> UacReading<'a> {
    fn sent_invite_on(&self, emitter: &str, call_id: &str, branch: &str) -> bool {
        self.txns
            .get(&UacKey { emitter, call_id, branch })
            .is_some_and(|txn| !txn.invites.is_empty())
    }

    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = UacReading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        if msg.repeat {
            return;
        }
        let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else { return };
        if let Some(status) = msg.status() {
            if status < 200 || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            self.txns.entry(UacKey::taken_by(msg, branch)).or_default().finals.push(Final {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
                cseq: msg.cseq,
                to_tag: msg.to_tag.as_deref().unwrap_or_default(),
            });
            return;
        }
        // The REQUEST LINE names the act; the CSeq NUMBER names the
        // transaction. Reading the method off the CSeq instead would let a
        // CANCEL that mis-spelled its own CSeq method (`cancel-cseq-method`,
        // a rule of its own) be read as something else entirely.
        let Kind::Request { method } = &msg.kind else { return };
        let emitted = Emitted { msg: mi, hop: msg.hop, ts_us: msg.at_us, taker: msg.dst.as_str() };
        let method = method.to_ascii_uppercase();
        if method == "ACK" {
            let dialog = AckDialog {
                emitter: msg.src.as_str(),
                call_id: msg.call_id.as_str(),
                cseq: msg.cseq,
                to_tag: msg.to_tag.as_deref().unwrap_or_default(),
            };
            self.acks_2xx.entry(dialog).or_default().push(SentAck { ack: emitted, branch });
        }
        let txn = self.txns.entry(UacKey::sent_by(msg, branch)).or_default();
        match method.as_str() {
            "INVITE" => txn.invites.push(emitted),
            "ACK" => txn.acks.push(emitted),
            "CANCEL" => txn.cancels.push(emitted),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
