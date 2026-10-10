//! RFC 3261 §17.2.1 — a server transaction emits exactly ONE final response.
//!
//! Once a final (≥ 200) has gone out on a server transaction, the only further
//! response that transaction may send is a retransmission of that same final
//! (§17.2.1 for a non-2xx, §13.3.1.4 for a 2xx). A second final with a
//! DIFFERENT status is the violation: the taker has already ACKed the first one
//! and moved on, so the second lands on a transaction that no longer exists.
//!
//! **The occasion is one server transaction, as ONE endpoint ANSWERED it**, and
//! it costs an occasion only once a second final exists: a transaction with a
//! single final has nothing to disagree with and is no occasion at all.
//!
//! **Five keys decide it, and the METHOD is load-bearing.** Branch + method is
//! the §17 transaction key, because a CANCEL shares its INVITE's branch (§9.1)
//! — a `200 OK (CANCEL)` and the `487 (INVITE)` behind it are two transactions
//! on one branch, not two finals on one. Emitter, Call-ID and CSeq number ride
//! along so a view carrying many calls, or a fixture reusing one hard-coded
//! branch, cannot fold two transactions into one key.
//!
//! **Every divergent status is reported exactly once, all on ONE finding.** The
//! evidence carries them in emission order, so a transaction answered 487 then
//! 480 then 486 is one occasion naming both offences — and a Timer-G
//! retransmission of an offending final adds nothing, since a status already
//! recorded is never recorded twice.
//!
//! **Repeats are not fresh events.** A message the adapter marked
//! [`Msg::repeat`] is skipped outright: retransmitting the final until it is
//! answered is the required behaviour (§17.2.1, §13.3.1.4), never a second
//! answer.
//!
//! **A 1xx is not a final and [`SingleFinalPerServerTxn`] never judges one** —
//! the provisional AFTER the final is its sibling [`No1xxAfterFinal`]
//! (§13.3.1.1), which shares the section and the transaction key but reads the
//! other half of "complete": nothing more goes out, provisionals included.
//!
//! **No branch at the vantage, no verdict.** A view whose finals carry no
//! top-Via branch cannot be split into transactions, so an endpoint that sent
//! more than one final there is `Undecidable` rather than guessed at.
//!
//! Charges the transaction's UAS side: the endpoint that SENT the finals.
//! Whether that endpoint merely relayed an upstream's answer, and which roles
//! are in subject, is consumer policy — the rule states only what the wire
//! shows one emitter doing.

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Msg, WireView};

use super::Obligation;

crate::registry::rules! {
    /// RFC 3261 §17.2.1: a server transaction emits exactly one final response — after the first,
    /// only a same-status retransmission is legal (§13.3.1.4 for a 2xx). Charges the transaction's
    /// UAS side.
    SingleFinalPerServerTxn = "single-final-per-server-txn";
    /// RFC 3261 §13.3.1.1 / §17.2.1: a server transaction that has answered emits no NEW
    /// provisional afterwards. Charges the transaction's UAS side; the general sibling of
    /// [`RuleId::SingleFinalPerServerTxn`].
    No1xxAfterFinal = "no-1xx-after-final";
}

/// **RFC 3261 §17.2.1 — one final per server transaction.** See the module doc.
///
/// Charges the endpoint that sent the finals. Answering once — or repeating
/// that same answer for as long as it goes unanswered — discharges it.
pub struct SingleFinalPerServerTxn;

impl Obligation for SingleFinalPerServerTxn {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, finals) in &seen.txns {
            // One answer is no disagreement: the obligation is only ever tested
            // by a SECOND final.
            let [first, second, ..] = &finals[..] else { continue };
            let head = |anchor: usize, taker: &str, decision| Finding {
                rule: RuleId::SingleFinalPerServerTxn,
                emitter: key.emitter.to_string(),
                taker: taker.to_string(),
                cseq: key.cseq,
                relayed: false,
                anchor,
                decision,
            };
            // Without a branch the finals cannot be split into transactions, so
            // this endpoint's several answers may be several transactions'.
            let Some(branch) = key.branch else {
                out.push(head(
                    second.msg,
                    second.taker,
                    Decision::Undecidable("no via branch at this vantage"),
                ));
                continue;
            };
            // Every status that disagreed with the first, in emission order and
            // each recorded once: a retransmitted offence is the same offence.
            let mut divergent: Vec<u16> = Vec::new();
            let mut offence: Option<&Final<'_>> = None;
            for f in &finals[1..] {
                if f.status == first.status {
                    continue;
                }
                if !divergent.contains(&f.status) {
                    divergent.push(f.status);
                }
                offence.get_or_insert(f);
            }
            let Some(offence) = offence else {
                out.push(head(second.msg, second.taker, Decision::Compliant));
                continue;
            };
            out.push(head(
                offence.msg,
                offence.taker,
                Decision::Violated(Evidence::MultipleFinals {
                    first_msg: first.msg,
                    first_hop: first.hop,
                    first_ts_us: first.ts_us,
                    first_status: first.status,
                    second_msg: offence.msg,
                    second_hop: offence.hop,
                    second_ts_us: offence.ts_us,
                    second_status: offence.status,
                    divergent,
                    gap_us: offence.ts_us.saturating_sub(first.ts_us),
                    method: key.method.clone(),
                    branch: branch.to_string(),
                }),
            ));
        }
        // Observation order, so a ladder and the report read the same way.
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §13.3.1.1 / §17.2.1 — a completed transaction emits no further
/// provisionals.** Once a UAS has answered an INVITE server transaction, the
/// offer is resolved and the transaction is complete; a NEW provisional after
/// that presents the caller an early dialog past the point it could mean
/// anything, and a strict UAC's dialog bookkeeping has nowhere to put it.
///
/// The occasion is ONE new provisional an endpoint SENT on an INVITE server
/// transaction. Charges that endpoint; sending it while the transaction was
/// still open — which is what a provisional is for — discharges it.
///
/// **A provisional is identified by `(status, To tag)` — the early dialog it
/// presents — not by the repeat mark.** That is what tells a NEW provisional
/// from a retransmitted one whose FIRST emission was pre-final, even where a
/// vantage records the copy after the final by reordering. A retransmitted
/// final repeats a completion already noted and changes nothing.
///
/// **A 100 Trying is never an occasion**: it establishes no early dialog, so
/// §17.2.1's replay of it to a retransmitted INVITE is exactly what is owed.
///
/// **No branch at the vantage, no occasion.** The branch is the transaction, and
/// without one nothing says which final a provisional came after.
///
/// This is the general sibling of RFC 3262's reliable-only rule, which keys on
/// `RSeq` instead; the two never judge one message twice, because a reliable
/// provisional's own §4 obligations are the PRACK family's.
pub struct No1xxAfterFinal;

impl Obligation for No1xxAfterFinal {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per (emitter, call, branch): the final that completed it, and the
        // early dialogs whose provisional this vantage has already carried.
        let mut completed: BTreeMap<(&str, &str, &str), (usize, u16, u64)> = BTreeMap::new();
        let mut emitted: BTreeSet<(&str, &str, &str, u16, &str)> = BTreeSet::new();
        let mut out = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(status) = msg.status() else { continue };
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                continue;
            }
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            let txn = (msg.src.as_str(), msg.call_id.as_str(), branch);
            if status >= 200 {
                // A retransmitted final repeats a completion already noted.
                completed.entry(txn).or_insert((mi, status, msg.at_us));
                continue;
            }
            // 100 Trying presents no early dialog: §17.2.1 has the transaction
            // replay it to a retransmitted INVITE, and that is not a new one.
            if status <= 100 {
                continue;
            }
            let to_tag = msg.to_tag.as_deref().unwrap_or_default();
            let ident = (txn.0, txn.1, txn.2, status, to_tag);
            // Already carried once: the same provisional again, wherever the
            // vantage happened to record the copy.
            if !emitted.insert(ident) {
                continue;
            }
            let decision = match completed.get(&txn) {
                // The transaction was still open: a provisional is exactly what
                // it is for.
                None => Decision::Compliant,
                Some(&(final_msg, final_status, final_ts_us)) => {
                    Decision::Violated(Evidence::LateProvisional {
                        late_provisional_msg: mi,
                        late_provisional_hop: msg.hop,
                        late_provisional_ts_us: msg.at_us,
                        status,
                        to_tag: to_tag.to_string(),
                        completed_by_msg: final_msg,
                        completed_by_status: final_status,
                        gap_us: msg.at_us.saturating_sub(final_ts_us),
                        branch: branch.to_string(),
                    })
                }
            };
            out.push(Finding {
                rule: RuleId::No1xxAfterFinal,
                emitter: msg.src.to_string(),
                taker: msg.dst.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}

/// Every final each endpoint sent on each server transaction, in emission
/// order.
#[derive(Debug, Default)]
struct Reading<'a> {
    txns: BTreeMap<TxnKey<'a>, Vec<Final<'a>>>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// Absorb one message: a fresh final response the message's SENDER emitted.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        if msg.repeat {
            return;
        }
        let Some(status) = msg.status().filter(|s| *s >= 200) else { return };
        self.txns.entry(TxnKey::emitted_by(msg)).or_default().push(Final {
            msg: mi,
            hop: msg.hop,
            ts_us: msg.at_us,
            status,
            taker: msg.dst.as_str(),
        });
    }
}

/// One server transaction as ONE endpoint answered it: the §17 transaction key
/// (branch + method), the call it rides and the CSeq number it answers.
/// Spelled out as a struct so the five parts can never swap places.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    /// The endpoint judged: the UAS side, which emits the transaction's finals.
    emitter: &'a str,
    call_id: &'a str,
    /// The top-Via branch, or `None` where this vantage carried none.
    branch: Option<&'a str>,
    cseq: u32,
    /// The CSeq method, ASCII-uppercased: a CANCEL's transaction is not its
    /// INVITE's even though the two share a branch.
    method: String,
}

impl<'a> TxnKey<'a> {
    /// The transaction `msg`'s SENDER is the UAS side of — a response going out.
    fn emitted_by(msg: &'a Msg) -> Self {
        TxnKey {
            emitter: msg.src.as_str(),
            call_id: msg.call_id.as_str(),
            branch: msg.via_branch.as_deref(),
            cseq: msg.cseq,
            method: msg.cseq_method.to_ascii_uppercase(),
        }
    }
}

/// One final response an endpoint emitted on a transaction.
#[derive(Debug)]
struct Final<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    taker: &'a str,
}

#[cfg(test)]
mod tests;
