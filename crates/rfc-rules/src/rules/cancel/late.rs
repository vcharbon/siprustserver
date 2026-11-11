//! RFC 3261 §9.1 — a CANCEL after its INVITE client transaction completed,
//! read as the UAC drove the transaction.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::super::branch::BranchName;
use super::super::sides::Sides;
use super::super::Obligation;
use super::keys::UacKey;

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
            .get(&UacKey { emitter, call_id, branch: BranchName(branch) })
            .is_some_and(|txn| !txn.invites.is_empty())
    }

    fn of(msgs: &'a [Msg]) -> Self {
        let sides = Sides::of(msgs);
        let mut seen = UacReading::default();
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
        if let Some(status) = msg.status() {
            if status < 200 || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            self.txns.entry(UacKey::taken_by(sides, msg, branch)).or_default().finals.push(Final {
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
