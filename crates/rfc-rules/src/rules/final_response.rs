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

/// **RFC 3261 §17.2.1 — one final per server transaction.** See the module doc.
///
/// Charges the endpoint that sent the finals. Answering once — or repeating
/// that same answer for as long as it goes unanswered — discharges it.
pub struct SingleFinalPerServerTxn;

impl Obligation for SingleFinalPerServerTxn {
    fn id(&self) -> RuleId {
        RuleId::SingleFinalPerServerTxn
    }

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
    fn id(&self) -> RuleId {
        RuleId::No1xxAfterFinal
    }

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
mod tests {
    //! The rule's OWN semantics under a CLOSED observation — what the live
    //! adapter's relay-lane and subject policy cannot state: which emissions
    //! form one transaction, and what the wire alone settles about them.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{No1xxAfterFinal, SingleFinalPerServerTxn};

    const UAC: &str = "10.0.0.1:5060";
    const UAS: &str = "10.0.0.2:5060";

    /// A response the UAS sent on `branch` for a `method` transaction.
    fn rsp(at_us: u64, status: u16, branch: &str, method: &str) -> Msg {
        Msg {
            at_us,
            src: UAS.to_string(),
            dst: UAC.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: method.to_string(),
            from_tag: Some("fa".to_string()),
            to_tag: Some("tb".to_string()),
            via_branch: Some(branch.to_string()),
            head: None,
            body: None,
        }
    }

    /// The INVITE-transaction response every test but the method ones uses.
    fn inv(at_us: u64, status: u16) -> Msg {
        rsp(at_us, status, "z9hG4bK-1", "INVITE")
    }

    /// The same message again on the wire — what §17.2.1 obliges, not a second
    /// answer.
    fn again(mut m: Msg) -> Msg {
        m.repeat = true;
        m
    }

    fn on_call(mut m: Msg, call_id: &str) -> Msg {
        m.call_id = call_id.to_string();
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

    fn eval(msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
        SingleFinalPerServerTxn.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// The violation: the transaction answered 487, the caller ACKed and moved
    /// on, and a later decision authored a 480 on the same transaction.
    #[test]
    fn a_second_final_of_another_status_is_violated() {
        let f = eval(&[inv(1_000, 487), inv(3_000, 480)]);
        assert_eq!(f.len(), 1, "one occasion, the transaction: {f:?}");
        assert_eq!(f[0].rule, RuleId::SingleFinalPerServerTxn);
        assert_eq!(f[0].emitter, UAS, "the endpoint that answered twice is charged");
        assert_eq!(f[0].taker, UAC);
        assert!(!f[0].relayed);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the divergent final");
        let Decision::Violated(Evidence::MultipleFinals {
            first_status,
            second_status,
            divergent,
            gap_us,
            method,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("multiple-finals evidence: {:?}", f[0].decision)
        };
        assert_eq!((*first_status, *second_status), (487, 480));
        assert_eq!(divergent.as_slice(), [480]);
        assert_eq!(*gap_us, 2_000);
        assert_eq!((method.as_str(), branch.as_str()), ("INVITE", "z9hG4bK-1"));
    }

    /// One answer has nothing to disagree with: no occasion at all.
    #[test]
    fn a_single_final_is_no_occasion() {
        assert!(eval(&[inv(1_000, 200)]).is_empty());
    }

    /// §17.2.1 Timer G repeats the non-2xx final and §13.3.1.4 the 2xx — a
    /// message marked as that repeat is the same answer, so the transaction
    /// still has only one and is no occasion.
    #[test]
    fn a_retransmitted_final_is_not_a_second_answer() {
        let f = eval(&[inv(1_000, 486), again(inv(2_000, 486)), again(inv(3_000, 486))]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// A second final the vantage did NOT mark a repeat still costs an
    /// occasion — and answering the same status again is compliant, the
    /// denominator a report reads the violation rate against.
    #[test]
    fn a_second_final_of_the_same_status_is_compliant() {
        let f = eval(&[inv(1_000, 486), inv(2_000, 486)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// Every divergent status is reported exactly once, ALL on one finding: the
    /// occasion is the transaction, so a retransmitted offence adds nothing and
    /// a genuinely third status joins the same evidence rather than opening a
    /// second occasion.
    #[test]
    fn every_divergent_status_is_reported_exactly_once_on_one_finding() {
        let retransmitted =
            eval(&[inv(1_000, 487), inv(2_000, 480), again(inv(3_000, 480))]);
        assert_eq!(retransmitted.len(), 1, "{retransmitted:?}");
        let Decision::Violated(Evidence::MultipleFinals { divergent, .. }) =
            &retransmitted[0].decision
        else {
            panic!("{:?}", retransmitted[0].decision)
        };
        assert_eq!(divergent.as_slice(), [480], "one recording of the offending status");
        assert_eq!(retransmitted[0].anchor, 1, "the FIRST copy of the offending final");

        let three = eval(&[
            inv(1_000, 487),
            inv(2_000, 480),
            again(inv(3_000, 480)),
            inv(4_000, 486),
        ]);
        assert_eq!(three.len(), 1, "a third status joins the occasion: {three:?}");
        let Decision::Violated(Evidence::MultipleFinals { divergent, second_status, .. }) =
            &three[0].decision
        else {
            panic!("{:?}", three[0].decision)
        };
        assert_eq!(divergent.as_slice(), [480, 486], "both, in emission order");
        assert_eq!(*second_status, 480, "the offence is where the answers first parted");
        assert_eq!(three[0].anchor, 1);
    }

    /// A repeat the vantage failed to mark is still one recording: the same
    /// status never enters the evidence twice.
    #[test]
    fn an_unmarked_duplicate_offence_is_recorded_once() {
        let f = eval(&[inv(1_000, 487), inv(2_000, 480), inv(3_000, 480)]);
        let Decision::Violated(Evidence::MultipleFinals { divergent, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(divergent.as_slice(), [480]);
    }

    /// A 1xx is not a final: the transaction below answered once, and the
    /// provisional that followed is a different obligation's finding.
    #[test]
    fn a_provisional_after_the_final_is_not_judged() {
        assert!(eval(&[inv(1_000, 200), inv(2_000, 183)]).is_empty());
    }

    /// A CANCEL reuses its INVITE's branch (§9.1): the `200 (CANCEL)` and the
    /// `487 (INVITE)` are two server transactions, each answered once — folding
    /// them on the branch alone would invent a violation.
    #[test]
    fn a_cancel_200_and_its_invite_487_are_two_transactions() {
        let f = eval(&[
            rsp(1_000, 200, "z9hG4bK-c", "CANCEL"),
            rsp(2_000, 487, "z9hG4bK-c", "INVITE"),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// A view carries many calls, and fixtures reuse hard-coded branches: the
    /// Call-ID keeps two singly-answered transactions from folding into one.
    #[test]
    fn two_calls_reusing_one_branch_are_distinct_transactions() {
        let f = eval(&[
            on_call(inv(1_000, 200), "call-a"),
            on_call(inv(2_000, 486), "call-b"),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    /// §17.2.2: the one-final rule is not INVITE-specific — a BYE server
    /// transaction answered twice with different statuses is a violation.
    #[test]
    fn a_non_invite_server_transaction_is_judged() {
        let f = eval(&[
            rsp(1_000, 200, "z9hG4bK-n", "BYE"),
            rsp(2_000, 481, "z9hG4bK-n", "BYE"),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{:?}", f[0].decision);
    }

    /// Without a branch the finals cannot be split into transactions, so an
    /// endpoint that answered more than once at this vantage is UNDECIDABLE —
    /// the rule never guesses which answers belonged together.
    #[test]
    fn finals_without_a_via_branch_are_undecidable() {
        let branchless = |m: Msg| {
            let mut m = m;
            m.via_branch = None;
            m
        };
        let f = eval(&[branchless(inv(1_000, 487)), branchless(inv(2_000, 480))]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
            "{:?}",
            f[0].decision
        );
        assert!(!f[0].decided());

        // One branchless final is no occasion either: there is nothing the
        // branch would have had to settle.
        assert!(eval(&[branchless(inv(1_000, 200))]).is_empty());
    }

    // ── No1xxAfterFinal (RFC 3261 §13.3.1.1 / §17.2.1) ──────────────────────

    /// A provisional presenting a chosen early dialog.
    fn early(at_us: u64, status: u16, to_tag: &str) -> Msg {
        let mut m = inv(at_us, status);
        m.to_tag = Some(to_tag.to_string());
        m
    }

    fn late_1xx(msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
        No1xxAfterFinal.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// The violation: 180, then the 200 that resolved the offer, then a NEW 181
    /// on a transaction that is already complete.
    #[test]
    fn a_new_provisional_after_the_final_is_violated() {
        let f = late_1xx(&[early(1_000, 180, "bt"), inv(2_000, 200), early(3_000, 181, "bt")]);
        assert_eq!(f.len(), 2, "both provisionals are occasions: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[1].rule, RuleId::No1xxAfterFinal);
        assert_eq!(f[1].emitter, UAS, "the transaction's UAS side is charged");
        assert_eq!(f[1].anchor, 2, "the occasion rests on the late provisional");
        let Decision::Violated(Evidence::LateProvisional {
            status,
            completed_by_status,
            completed_by_msg,
            gap_us,
            branch,
            ..
        }) = &f[1].decision
        else {
            panic!("{:?}", f[1].decision)
        };
        assert_eq!((*status, *completed_by_status, *completed_by_msg), (181, 200, 1));
        assert_eq!(*gap_us, 1_000);
        assert_eq!(branch.as_str(), "z9hG4bK-1");
    }

    /// A provisional is identified by `(status, To tag)`, so a retransmitted
    /// final and a retransmitted provisional — one first emitted PRE-final,
    /// whose copy the vantage recorded after — never fire.
    #[test]
    fn retransmitted_finals_and_provisionals_are_silent() {
        assert!(late_1xx(&[
            early(1_000, 180, "bt"),
            inv(2_000, 200),
            inv(3_000, 200),
            early(4_000, 180, "bt"),
        ])
        .iter()
        .all(|f| !f.violated()));
    }

    /// Two distinct early dialogs before the winner: the multi-fork happy
    /// path, both compliant.
    #[test]
    fn several_early_dialogs_before_the_final_are_compliant() {
        let f = late_1xx(&[early(1_000, 180, "bt1"), early(2_000, 183, "bt2"), inv(3_000, 200)]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{f:?}");
    }

    /// A 100 Trying establishes no early dialog: §17.2.1 has the transaction
    /// replay it to a retransmitted INVITE, so it is never an occasion.
    #[test]
    fn a_100_trying_after_the_final_is_not_judged() {
        assert!(late_1xx(&[inv(1_000, 200), inv(2_000, 100)]).is_empty());
    }

    /// Two calls reusing one branch are two transactions: a provisional on the
    /// second is not "after" the first's final.
    #[test]
    fn two_calls_reusing_one_branch_do_not_fold() {
        let f = late_1xx(&[
            on_call(inv(1_000, 200), "call-a"),
            on_call(early(2_000, 180, "bt"), "call-b"),
        ]);
        assert!(f.iter().all(|f| !f.violated()), "{f:?}");
    }

    /// Without a branch nothing says which final a provisional came after: no
    /// occasion at all.
    #[test]
    fn a_branchless_provisional_is_no_occasion() {
        let mut late = early(2_000, 180, "bt");
        late.via_branch = None;
        assert!(late_1xx(&[inv(1_000, 200), late]).is_empty());
    }
}
