//! RFC 3261 §9 — the CANCEL family: what a UAC must have heard before it
//! cancels and what its CANCEL must carry (§9.1), and how the UAS must answer
//! a cancelled INVITE (§9.2).
//!
//! Two readings serve the three rules, because the two sides of a cancellation
//! key on different facts. The §9.2 rule reads the cancelled TRANSACTION as
//! its UAS took it (call + CSeq number + direction), and that reading lives
//! here. The two §9.1 rules read the BRANCH as its UAC drove it (a CANCEL
//! reuses its INVITE's top-Via branch) — the walk every branch-pairing family
//! shares, in [`super::branch`].
//!
//! **Repeats are not fresh events** in any of the three: a message the adapter
//! marked [`Msg::repeat`] opens no occasion, since retransmitting until
//! answered is required behaviour, never a second act.
//!
//! **A message with no top-Via branch keys no §9.1 occasion.** The branch is
//! how §9.1 pairs a CANCEL with its INVITE; without one the pair cannot be
//! formed, and the rule states nothing rather than guessing at a pairing.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::branch::BranchReading;
use super::Obligation;

/// **RFC 3261 §9.2 — no 2xx after a CANCEL.**
///
/// **The occasion is the cancelled TRANSACTION, as ONE endpoint saw it**: the
/// endpoint took the INVITE, then took a CANCEL naming that INVITE's CSeq
/// number. Both facts are on the wire, so every occasion is DECIDED — this rule
/// has no absence to wait out and no undecidable verdict, and a closed
/// observation reads exactly like an open one.
///
/// **Three keys decide it: the call, the CSeq number, and DIRECTION.** A
/// CANCEL names its INVITE by Call-ID and CSeq number (§9.1), and a view can
/// carry many calls — a live bind serves every dialog on its socket — so the
/// CSeq number alone would fuse two calls' transactions. Direction is the
/// third: the two directions of a dialog number their CSeqs independently and a
/// callee's own re-INVITE can carry the number the initial INVITE did, so an
/// endpoint is only measured against the INVITEs it TOOK and the responses it
/// EMITTED.
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
        for (&TxnKey { emitter, call_id: _, cseq }, txn) in &seen.txns {
            // Only the UAS side of a transaction this vantage actually saw
            // opened is judged: an endpoint measured against an INVITE nobody
            // observed it take is measured against nothing.
            let Some(cancel) = txn.cancel.as_ref().filter(|_| txn.took_invite) else { continue };
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
    /// The INVITE transaction each endpoint saw, keyed as §9.1 names it.
    txns: BTreeMap<TxnKey<'a>, Txn<'a>>,
    /// (transaction, To tag) → this dialog's final was already seen.
    answered: BTreeSet<(TxnKey<'a>, &'a str)>,
    /// (call, CSeq number, To tag) → when this response first crossed ANY
    /// vantage of the view. A later emission of it is a relay, whatever address
    /// the relaying box wears on its far interface.
    first_seen: BTreeMap<(&'a str, u32, &'a str), u64>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// Absorb one message.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        if msg.repeat {
            return;
        }
        if msg.is_request("INVITE") {
            self.txns.entry(TxnKey::taken_by(msg)).or_default().took_invite = true;
        } else if msg.is_request("CANCEL") {
            // A CANCEL carries the CSeq NUMBER of the INVITE it cancels; the
            // earliest arrival is when the UAS took it.
            self.txns.entry(TxnKey::taken_by(msg)).or_default().cancel.get_or_insert(Cancel {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                src: msg.src.as_str(),
            });
        } else if let Some(status) = msg.status() {
            if !(200..300).contains(&status) || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            let txn = TxnKey::emitted_by(msg);
            let to_tag = msg.to_tag.as_deref().unwrap_or_default();
            let originated_at = *self
                .first_seen
                .entry((msg.call_id.as_str(), msg.cseq, to_tag))
                .or_insert(msg.at_us);
            if !self.answered.insert((txn, to_tag)) {
                return; // a second 2xx of a dialog already answered
            }
            self.txns.entry(txn).or_default().answers.push(Answer {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
                taker: msg.dst.as_str(),
                relayed: originated_at < msg.at_us,
            });
        }
    }
}

/// One INVITE transaction as ONE endpoint saw it: the call it rides, its CSeq
/// number, and which endpoint is its UAS side. Spelled out as a struct so the
/// three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    /// The endpoint judged: the UAS that TOOK the INVITE and emits its answers.
    emitter: &'a str,
    call_id: &'a str,
    cseq: u32,
}

impl<'a> TxnKey<'a> {
    /// The transaction `msg`'s RECIPIENT is the UAS side of — an INVITE or its
    /// CANCEL arriving.
    fn taken_by(msg: &'a Msg) -> Self {
        TxnKey { emitter: msg.dst.as_str(), call_id: msg.call_id.as_str(), cseq: msg.cseq }
    }

    /// The transaction `msg`'s SENDER is the UAS side of — a response going out.
    fn emitted_by(msg: &'a Msg) -> Self {
        TxnKey { emitter: msg.src.as_str(), call_id: msg.call_id.as_str(), cseq: msg.cseq }
    }
}

/// What one endpoint saw of one transaction: whether it took the INVITE, the
/// CANCEL it took, and the 2xxs it emitted.
#[derive(Debug, Default)]
struct Txn<'a> {
    /// This endpoint received the INVITE: it is the transaction's UAS side.
    took_invite: bool,
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
/// Known blind spots of the timing test, tolerable because the finding is
/// advisory and the layer tests carry the real regression protection: a
/// teardown flush inside the grace window sends under the floor on correct
/// behaviour and is noted; and an endpoint with no hold machinery at all that
/// happens to CANCEL past the floor is accepted without a finding.
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
/// the CSeq number and DIRECTION, the reading [`No200AfterCancel`] uses from
/// the other side. §9.1 has a CANCEL name its INVITE by Call-ID and CSeq
/// number, which is what a vantage carrying no top-Via branch can still pair
/// on.
///
/// **The emitter's own ACK is what decides it.** A final observed just before
/// the CANCEL may have crossed it in flight, and a capture point away from the
/// emitter can order the two wrongly; an ACK the EMITTER sent for that final
/// (§17.1.1.3, reusing the INVITE's CSeq number) cannot have crossed anything.
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
                // The ATTEMPT the CANCEL names is the emitter's last INVITE
                // ahead of it: a serial hunt reuses the CSeq number, so a final
                // that ended an EARLIER attempt says nothing about this one. An
                // INVITE this vantage never saw it send is no occasion at all.
                let Some(invite) = txn.invites.iter().rev().find(|i| i.msg < cancel.msg) else {
                    continue;
                };
                let finding = |decision| Finding {
                    rule: RuleId::NoCancelAfterFinal,
                    emitter: key.emitter.to_string(),
                    taker: cancel.taker.to_string(),
                    cseq: key.cseq,
                    relayed: false,
                    anchor: cancel.msg,
                    decision,
                };
                out.push(finding(match completed(txn, invite, cancel) {
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
/// final the emitter took, and the ACK it sent for that final before
/// cancelling. `None` wherever either is missing or out of order, which is
/// every compliant CANCEL and every crossing this vantage cannot separate.
fn completed(txn: &UacTxn<'_>, invite: &Emitted<'_>, cancel: &Emitted<'_>) -> Option<Evidence> {
    let fin = txn.finals.iter().find(|f| f.msg > invite.msg && f.msg < cancel.msg)?;
    let ack = txn.acks.iter().find(|a| a.msg > fin.msg && a.msg < cancel.msg)?;
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

/// One INVITE transaction as ONE endpoint DROVE it: the call it rides, its CSeq
/// number, and which endpoint is its UAC side. The mirror of [`TxnKey`], which
/// names the same transaction by its UAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct UacKey<'a> {
    /// The endpoint judged: the UAC that SENT the INVITE and its CANCEL.
    emitter: &'a str,
    call_id: &'a str,
    cseq: u32,
}

impl<'a> UacKey<'a> {
    /// The transaction `msg`'s SENDER drives — a request going out.
    fn sent_by(msg: &'a Msg) -> Self {
        UacKey { emitter: msg.src.as_str(), call_id: msg.call_id.as_str(), cseq: msg.cseq }
    }

    /// The transaction `msg`'s RECIPIENT drives — a response coming back.
    fn taken_by(msg: &'a Msg) -> Self {
        UacKey { emitter: msg.dst.as_str(), call_id: msg.call_id.as_str(), cseq: msg.cseq }
    }
}

/// What one endpoint did under one (call, CSeq number) as UAC, in view order:
/// the INVITEs it sent, the finals it took, and the ACKs and CANCELs it sent.
/// Every list, because a serial hunt spends one CSeq number on several
/// attempts and each is its own transaction.
#[derive(Debug, Default)]
struct UacTxn<'a> {
    invites: Vec<Emitted<'a>>,
    finals: Vec<Final>,
    acks: Vec<Emitted<'a>>,
    cancels: Vec<Emitted<'a>>,
}

/// One request an endpoint sent on the transaction, and where it went.
#[derive(Debug)]
struct Emitted<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    taker: &'a str,
}

/// The first final an endpoint took for its own INVITE.
#[derive(Debug)]
struct Final {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
}

/// What one view's messages say about the transactions each endpoint drove as
/// UAC. Repeats are skipped: retransmitting until answered is required
/// behaviour, never a second act.
#[derive(Debug, Default)]
struct UacReading<'a> {
    txns: BTreeMap<UacKey<'a>, UacTxn<'a>>,
}

impl<'a> UacReading<'a> {
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
        if let Some(status) = msg.status() {
            if status < 200 || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            self.txns.entry(UacKey::taken_by(msg)).or_default().finals.push(Final {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
            });
            return;
        }
        // The REQUEST LINE names the act; the CSeq NUMBER names the
        // transaction. Reading the method off the CSeq instead would let a
        // CANCEL that mis-spelled its own CSeq method (`cancel-cseq-method`,
        // a rule of its own) be read as something else entirely.
        let Kind::Request { method } = &msg.kind else { return };
        let emitted = Emitted { msg: mi, hop: msg.hop, ts_us: msg.at_us, taker: msg.dst.as_str() };
        let txn = self.txns.entry(UacKey::sent_by(msg)).or_default();
        match method.to_ascii_uppercase().as_str() {
            "INVITE" => txn.invites.push(emitted),
            "ACK" => txn.acks.push(emitted),
            "CANCEL" => txn.cancels.push(emitted),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The family's OWN semantics under a CLOSED observation — what the capture
    //! conformance pins in `sip_pcap::rfc::cancel` and the live adapter's lane
    //! policy cannot state: the crossing race, how a branch pairs a CANCEL with
    //! its INVITE, and what the wire alone settles about each pairing.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        CancelAfter1xx, CancelRouteEchoesInvite, No200AfterCancel, NoCancelAfterFinal,
        CANCEL_GRACE_FLOOR_US,
    };

    const UAC: &str = "10.0.0.1:5060";
    const UAS: &str = "10.0.0.2:5060";

    fn msg(at_us: u64, src: &str, dst: &str, kind: Kind, cseq_method: &str) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind,
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: cseq_method.to_string(),
            via_branch: None,
            from_tag: Some("fa".to_string()),
            to_tag: None,
            head: None,
            body: None,
        }
    }

    fn req(at_us: u64, method: &str) -> Msg {
        msg(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, method)
    }

    fn rsp(at_us: u64, status: u16) -> Msg {
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, "INVITE");
        m.to_tag = Some("tb".to_string());
        m
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    /// The violation, under a closed observation: the UAS took the CANCEL and
    /// answered 200 anyway. No window collapses here — the offence is a message
    /// PRESENT on the wire, so closing the observation changes nothing.
    #[test]
    fn a_2xx_after_the_cancel_is_violated_under_a_closed_observation() {
        let msgs =
            vec![req(1_000, "INVITE"), rsp(3_000, 180), req(4_000, "CANCEL"), rsp(6_000, 200)];
        let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "one occasion, the cancelled transaction: {f:?}");
        assert_eq!(f[0].rule, RuleId::No200AfterCancel);
        assert_eq!(f[0].emitter, UAS, "the UAS that answered is charged");
        assert_eq!(f[0].taker, UAC);
        assert!(!f[0].relayed);
        let Decision::Violated(Evidence::Cancelled { gap_us, status, .. }) = &f[0].decision else {
            panic!("cancel evidence: {:?}", f[0].decision)
        };
        assert_eq!((*gap_us, *status), (2_000, 200));
    }

    /// The crossing race stays COMPLIANT live: the 200 was already on the wire
    /// when the CANCEL arrived, so the UAS had nothing to answer 487 to. The
    /// occasion is still decided — the denominator a report reads the violation
    /// rate against.
    #[test]
    fn a_2xx_that_crossed_the_cancel_stays_compliant_live() {
        let msgs =
            vec![req(1_000, "INVITE"), rsp(3_000, 180), rsp(4_000, 200), req(4_500, "CANCEL")];
        let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "the crossing costs an occasion: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].taker, UAC, "the party that sent the CANCEL is owed the 487");
    }

    /// The violation: the UAC took a final, ACKed it, and CANCELled the
    /// transaction it had just completed. The 481 the far side owes such a
    /// CANCEL is what the rule exists to predict.
    #[test]
    fn a_cancel_after_the_uac_acked_its_own_final_is_violated() {
        let msgs = vec![
            req(1_000, "INVITE"),
            rsp(3_000, 180),
            rsp(4_000, 480),
            req(4_500, "ACK"),
            req(5_000, "CANCEL"),
        ];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
        assert_eq!(f[0].emitter, UAC, "the UAC that cancelled is charged");
        assert_eq!(f[0].taker, UAS);
        let Decision::Violated(Evidence::LateCancel {
            since_final_us, final_status, ack_msg, ..
        }) = &f[0].decision
        else {
            panic!("late-cancel evidence: {:?}", f[0].decision)
        };
        assert_eq!((*since_final_us, *final_status, *ack_msg), (1_000, 480, 3));
    }

    /// The ordinary cancellation stays COMPLIANT: the transaction was still in
    /// flight, which is the only state §9.1 lets a UAC cancel from. The
    /// occasion is still decided — the denominator a report reads the rate
    /// against.
    #[test]
    fn a_cancel_before_the_final_stays_compliant() {
        let msgs = vec![
            req(1_000, "INVITE"),
            rsp(3_000, 180),
            req(4_000, "CANCEL"),
            rsp(5_000, 487),
            req(5_500, "ACK"),
        ];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "the ordinary cancel costs an occasion: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The crossing the rule refuses to charge: the final is in view before the
    /// CANCEL, but the emitter never ACKed it, so nothing proves it had the
    /// final in hand rather than passing it in flight. Under-reporting is the
    /// contract.
    #[test]
    fn a_final_the_emitter_never_acked_is_a_crossing_not_a_violation() {
        let msgs = vec![req(1_000, "INVITE"), rsp(4_000, 480), req(4_100, "CANCEL")];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A serial hunt spends ONE CSeq number on several attempts. The final that
    /// ended the first says nothing about the second, so the occasion is scoped
    /// to the emitter's LAST INVITE ahead of the CANCEL.
    #[test]
    fn a_final_of_an_earlier_attempt_does_not_charge_the_next_ones_cancel() {
        let msgs = vec![
            req(1_000, "INVITE"),
            rsp(2_000, 408),
            req(2_100, "ACK"),
            req(2_200, "INVITE"),
            rsp(3_000, 180),
            req(4_000, "CANCEL"),
        ];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A CANCEL whose INVITE this vantage never saw the endpoint SEND is no
    /// occasion: nothing says that endpoint is the transaction's UAC.
    #[test]
    fn a_cancel_without_a_witnessed_outgoing_invite_is_not_an_occasion() {
        let msgs = vec![rsp(4_000, 480), req(4_500, "ACK"), req(5_000, "CANCEL")];
        let f = NoCancelAfterFinal.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert!(f.is_empty(), "{f:?}");
    }

    /// A CANCEL for an INVITE this vantage never saw the endpoint take is no
    /// occasion at all: nothing says that endpoint is the transaction's UAS.
    #[test]
    fn a_cancel_without_a_witnessed_invite_is_not_an_occasion() {
        let msgs = vec![req(4_000, "CANCEL"), rsp(6_000, 200)];
        let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert!(f.is_empty(), "{f:?}");
    }

    /// The live vantage carries MANY calls on one socket, and a UAS numbers
    /// every call's initial INVITE CSeq 1. A CANCEL of one call says nothing
    /// about the next call's answer: the Call-ID keeps the two transactions
    /// apart, so the second call is no occasion of this rule at all.
    #[test]
    fn a_cancel_on_one_call_does_not_charge_the_next_calls_answer() {
        let on_call = |mut m: Msg, call_id: &str| {
            m.call_id = call_id.to_string();
            m
        };
        let msgs = vec![
            on_call(req(1_000, "INVITE"), "call-1"),
            on_call(rsp(3_000, 180), "call-1"),
            on_call(req(4_000, "CANCEL"), "call-1"),
            on_call(rsp(4_500, 487), "call-1"),
            // A second call to the same UAS socket, numbering its own CSeq 1.
            on_call(req(5_000, "INVITE"), "call-2"),
            on_call(rsp(6_000, 200), "call-2"),
        ];
        let f = No200AfterCancel.eval(&WireView { msgs: &msgs, obs: &obs(&msgs) });
        assert_eq!(f.len(), 1, "only the cancelled call is an occasion: {f:?}");
        assert!(
            matches!(f[0].decision, Decision::Compliant),
            "the cancelled call answered 487: {:?}",
            f[0].decision
        );
    }

    // ---- §9.1: the UAC's half of a cancellation --------------------------

    /// A request the UAC SENT on `branch`, carrying `extra` header rows in the
    /// head the §9.1 rules read.
    fn sent(at_us: u64, method: &str, branch: &str, extra: &str) -> Msg {
        let head = format!(
            "{method} sip:bob@10.0.0.2 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=fa\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n\
             {extra}\r\n"
        );
        let mut m = msg(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, method);
        m.via_branch = Some(branch.to_string());
        m.head = Some(head.into_bytes());
        m
    }

    /// A response the UAC TOOK back on `branch`.
    fn took(at_us: u64, status: u16, branch: &str) -> Msg {
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, "INVITE");
        m.via_branch = Some(branch.to_string());
        m.to_tag = Some("tb".to_string());
        m
    }

    fn routes(msgs: &[Msg]) -> Vec<Finding> {
        CancelRouteEchoesInvite.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn waited(msgs: &[Msg]) -> Vec<Finding> {
        CancelAfter1xx.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    const P1: &str = "Route: <sip:p1@h;lr>\r\n";
    const P2: &str = "Route: <sip:p2@h;lr>\r\n";

    /// The obligation met: the CANCEL carries the INVITE's Route rows, so it
    /// walks the path the INVITE walked.
    #[test]
    fn a_cancel_echoing_the_invites_route_is_compliant() {
        let f = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", P1),
            sent(4_000, "CANCEL", "z9hG4bK-1", P1),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the sender of the CANCEL is charged");
        assert_eq!(f[0].taker, UAS);
    }

    /// The violation: the CANCEL states a different path from the INVITE's, and
    /// the evidence names both sets and the branch that paired them.
    #[test]
    fn a_cancel_stating_another_path_is_violated() {
        let f = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", P1),
            sent(4_000, "CANCEL", "z9hG4bK-1", P2),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::CancelRouteEchoesInvite);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the CANCEL");
        assert!(!f[0].relayed);
        let Decision::Violated(Evidence::CancelRouteDiverged {
            cancel_routes,
            invite_routes,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("cancel-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(cancel_routes.as_slice(), ["<sip:p2@h;lr>"]);
        assert_eq!(invite_routes.as_slice(), ["<sip:p1@h;lr>"]);
        assert_eq!(branch, "z9hG4bK-1");
    }

    /// The Route ROWS are compared in order: the same hops re-folded into one
    /// row, or listed the other way round, are a different path statement, and
    /// §9.1 asks the CANCEL for a copy.
    #[test]
    fn route_rows_are_compared_in_wire_order() {
        let folded = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
            sent(4_000, "CANCEL", "z9hG4bK-1", "Route: <sip:p1@h;lr>, <sip:p2@h;lr>\r\n"),
        ]);
        assert!(folded[0].violated(), "{:?}", folded[0].decision);
        let reversed = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
            sent(4_000, "CANCEL", "z9hG4bK-1", &format!("{P2}{P1}")),
        ]);
        assert!(reversed[0].violated(), "{:?}", reversed[0].decision);
        let copied = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", &format!("{P1}{P2}")),
            sent(4_000, "CANCEL", "z9hG4bK-1", &format!("{P1}{P2}")),
        ]);
        assert!(matches!(copied[0].decision, Decision::Compliant), "{:?}", copied[0].decision);
    }

    /// A CANCEL routeless like its INVITE echoes it exactly — an empty set is a
    /// set.
    #[test]
    fn two_routeless_messages_agree() {
        let f = routes(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(4_000, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A CANCEL this vantage never saw the same emitter open with an INVITE is
    /// no occasion of either §9.1 rule: nothing pairs with it.
    #[test]
    fn a_cancel_without_a_witnessed_invite_is_no_occasion() {
        let msgs = [sent(4_000, "CANCEL", "z9hG4bK-1", P1)];
        assert!(routes(&msgs).is_empty(), "{:?}", routes(&msgs));
        assert!(waited(&msgs).is_empty(), "{:?}", waited(&msgs));

        // The branch is what §9.1 pairs on: an INVITE on another branch of the
        // same call pairs with nothing either.
        let other =
            [sent(1_000, "INVITE", "z9hG4bK-other", P1), sent(4_000, "CANCEL", "z9hG4bK-1", P2)];
        assert!(routes(&other).is_empty(), "{:?}", routes(&other));
    }

    /// A vantage that carried no header block for one of the two messages
    /// settles nothing about what they said — UNDECIDABLE, never clean.
    #[test]
    fn a_pair_without_header_bytes_is_undecidable() {
        let headless = |mut m: Msg| {
            m.head = None;
            m
        };
        let f = routes(&[
            headless(sent(1_000, "INVITE", "z9hG4bK-1", P1)),
            sent(4_000, "CANCEL", "z9hG4bK-1", P2),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no header block at this vantage")),
            "{:?}",
            f[0].decision
        );
        assert!(!f[0].decided());
    }

    /// The §9.1 wait, honoured: a provisional was in hand when the CANCEL went
    /// out, so the UAS has a server transaction to match it against.
    #[test]
    fn a_cancel_after_a_taken_provisional_is_compliant() {
        let f = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            took(3_000, 180, "z9hG4bK-1"),
            sent(4_000, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the CANCEL: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The eager CANCEL: nothing had answered the INVITE and the sender's own
    /// grace window had not run out, so the UAS may not have built the server
    /// transaction the CANCEL needs.
    #[test]
    fn an_eager_cancel_before_any_provisional_is_violated() {
        let f = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(2_000, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::CancelAfter1xx);
        assert_eq!(f[0].emitter, UAC, "the sender of the CANCEL is charged");
        assert_eq!(f[0].anchor, 1);
        let Decision::Violated(Evidence::EagerCancel { since_invite_us, branch, .. }) =
            &f[0].decision
        else {
            panic!("eager-cancel evidence: {:?}", f[0].decision)
        };
        assert_eq!((*since_invite_us, branch.as_str()), (1_000, "z9hG4bK-1"));
    }

    /// Past the grace floor the pre-1xx CANCEL is the sanctioned grace-expiry
    /// send toward a response-less callee: compliant, and still an occasion —
    /// the denominator a report reads the eager-CANCEL rate against.
    #[test]
    fn a_cancel_at_the_grace_floor_is_compliant() {
        let f = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(1_000 + CANCEL_GRACE_FLOOR_US, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        // One microsecond under it is the eager send.
        let under = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(999 + CANCEL_GRACE_FLOOR_US, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert!(under[0].violated(), "{:?}", under[0].decision);
    }

    /// ORDER IN THE VIEW is the whole of the "before" test: a provisional that
    /// arrived after the CANCEL had already left does not excuse it — the
    /// sender could not have been waiting for it.
    #[test]
    fn a_provisional_after_the_cancel_does_not_excuse_it() {
        let f = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(2_000, "CANCEL", "z9hG4bK-1", ""),
            took(3_000, 180, "z9hG4bK-1"),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{:?}", f[0].decision);
    }

    /// A provisional belongs to the branch it came back on: one branch's 18x
    /// discharges nothing on another branch of the same call.
    #[test]
    fn a_provisional_on_another_branch_discharges_nothing() {
        let f = waited(&[
            sent(1_000, "INVITE", "z9hG4bK-1", ""),
            sent(1_100, "INVITE", "z9hG4bK-2", ""),
            took(2_000, 180, "z9hG4bK-1"),
            sent(3_000, "CANCEL", "z9hG4bK-2", ""),
            sent(3_100, "CANCEL", "z9hG4bK-1", ""),
        ]);
        assert_eq!(f.len(), 2, "one occasion per CANCEL: {f:?}");
        assert!(f[0].violated(), "the fork with no answer: {:?}", f[0].decision);
        assert_eq!(f[0].anchor, 3);
        assert!(matches!(f[1].decision, Decision::Compliant), "{:?}", f[1].decision);
    }

    /// A CANCEL the vantage marked a repeat is the same act again — §9.1 is
    /// tested once, by the copy that first left the sender.
    #[test]
    fn a_retransmitted_cancel_is_not_a_second_occasion() {
        let again = |mut m: Msg| {
            m.repeat = true;
            m
        };
        let msgs = [
            sent(1_000, "INVITE", "z9hG4bK-1", P1),
            sent(2_000, "CANCEL", "z9hG4bK-1", P2),
            again(sent(3_000, "CANCEL", "z9hG4bK-1", P2)),
        ];
        assert_eq!(routes(&msgs).len(), 1, "{:?}", routes(&msgs));
        assert_eq!(waited(&msgs).len(), 1, "{:?}", waited(&msgs));
    }
}
