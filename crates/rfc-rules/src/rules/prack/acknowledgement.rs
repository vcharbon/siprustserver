//! The UAC's half of RFC 3262 §4: a reliable provisional it took is PRACKed,
//! in `RSeq` order, naming the INVITE it answers; a 100 never is.

use std::collections::BTreeSet;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;
use super::records::{ObligationKey, Reliable};

/// How long an OPEN observation must run past an unPRACKed reliable
/// provisional, with no final seen, before the absence is charged rather than
/// read as truncation.
///
/// One second: RFC 3262 §3 has the UAS retransmit the provisional on the T1
/// ladder (500 ms, then 1 s, …) until the PRACK arrives, and §4 has the UAC
/// PRACK on receipt. A recording that runs a full second past the provisional
/// without a PRACK did not merely stop before it.
pub const PRACK_WINDOW_US: u64 = 1_000_000;

/// **RFC 3262 §4 — a UAC that took a reliable provisional answers it with a
/// PRACK whose `RAck` names it.**
///
/// The offence is an ABSENCE; each conservatism gate costs an occasion the
/// population counts as undecided rather than clean: the negotiation must be
/// witnessed, the charged endpoint must be the transaction's UAC, a PRACK
/// whose `RAck` will not parse contaminates its sender's obligations, and an
/// open observation with no final must run [`PRACK_WINDOW_US`] past the
/// provisional.
///
/// **The rule, one for every observation.** The occasion is a reliable
/// provisional taken while the INVITE awaits its final; a CANCEL ends nothing
/// (§4: every reliable provisional of a pending transaction is PRACKed). Only
/// a final reaching the UAC BEFORE the provisional leaves no occasion; a final
/// after it, however soon, decides the absence, while a PRACK crossing that
/// final in flight still discharges it. What differs is the END of an
/// observation with no final: a CLOSED one (the harness drained, nothing in
/// flight) decides the absence at once, an OPEN one only after
/// [`PRACK_WINDOW_US`], before which the stop is truncation.
///
/// A provisional out of its dialog's `RSeq` order is owed nothing: §4 forbids
/// PRACKing it. Nor is one whose early dialog its responder then ended with a
/// BYE: nothing rides that dialog after it (RFC 3261 §15.1.2).
///
/// Repeats collapse on the RSeq: one PRACK answers every retransmission of one
/// reliable provisional (§3), so an obligation is keyed by its early dialog's
/// To tag and its `RSeq`, never by datagram.
pub struct UnackedReliableProvisional;

impl Obligation for UnackedReliableProvisional {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);

        let mut out = Vec::new();
        for (key, p) in &seen.owed {
            let ObligationKey { uac, call, cseq, dialog, rseq } = *key;
            let head = |decision| Finding {
                rule: RuleId::UnackedReliableProvisional,
                emitter: uac.to_string(),
                taker: p.peer.to_string(),
                cseq,
                relayed: false, // a hop that forwarded the provisional never reaches Violated
                anchor: p.msg,
                decision,
            };
            if seen.acked_at.contains_key(&(cseq, dialog, rseq)) {
                out.push(head(Decision::Compliant));
                continue;
            }
            let final_us = seen.final_at.get(&(uac, call, cseq)).copied();
            if final_us.is_some_and(|at| at <= p.ts_us) {
                continue; // the final came first: nothing was left to PRACK
            }
            let byed_early = seen
                .byed_at
                .get(&(uac, call, dialog))
                .is_some_and(|at| *at >= p.ts_us && final_us.is_none_or(|final_at| *at < final_at));
            if byed_early {
                continue; // its responder's BYE ended the early dialog under it
            }
            let arrivals: Vec<&Reliable<'_>> = seen
                .taken
                .get(&(uac, dialog))
                .map(|list| list.iter().filter(|r| r.cseq == cseq && r.ts_us <= p.ts_us).collect())
                .unwrap_or_default();
            if rseq_order(&arrivals).1.contains(&rseq) {
                out.push(head(Decision::Compliant)); // §4: out of order, never PRACKed
                continue;
            }
            let alive_until = final_us.unwrap_or(seen.last_ts_us).min(seen.last_ts_us);
            let window_us = alive_until - p.ts_us.min(alive_until);
            // The two hop tests, each the other's blind spot: an INVITE this
            // endpoint did not open, and a provisional it passed on.
            let from_tag = wire.msgs[p.msg].from_tag.as_deref().unwrap_or_default();
            let opened_it = seen
                .opened
                .get(&(uac, call, cseq))
                .is_some_and(|at| seen.invite_first_us.get(&(call, from_tag, cseq)) == Some(at));
            let forwarded_it =
                seen.propagated.get(&(uac, cseq, dialog, rseq)).is_some_and(|last| p.ts_us < *last);
            let undecidable = if !opened_it {
                Some("the charged endpoint did not open the INVITE (a hop is not a UAC)")
            } else if forwarded_it {
                Some("the endpoint passed the provisional on (a hop is not a UAC)")
            } else if !seen.invite.get(&(uac, cseq)).is_some_and(|i| i.offers_100rel) {
                Some("the INVITE did not offer 100rel — the negotiation was never witnessed")
            } else if seen.unreadable_prack.contains(uac) {
                Some("a PRACK the emitter sent has an unreadable RAck — it may be the one")
            } else if final_us.is_none() && !(wire.obs.closed || window_us >= PRACK_WINDOW_US) {
                Some("the observation stopped inside the window — truncation, not absence")
            } else {
                None
            };
            if let Some(reason) = undecidable {
                out.push(head(Decision::Undecidable(reason)));
                continue;
            }
            out.push(head(Decision::Violated(Evidence::Unacked {
                provisional_msg: p.msg,
                provisional_hop: p.hop,
                provisional_ts_us: p.ts_us,
                rseq,
                status: p.status,
                window_us,
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §7.2 — a PRACK's `RAck` CSeq-num names the INVITE the emitter
/// opened.** `RAck: response-num CSeq-num method` copies the CSeq-num and
/// method FROM the acknowledged provisional, naming the INVITE whose `RSeq`
/// space `response-num` indexes; they are not the PRACK's own CSeq. A PRACK
/// naming an INVITE its sender never opened matches no reliable transaction,
/// so it settles nothing and the provisional keeps retransmitting.
///
/// Charges the PRACK's SENDER. A sender that opened no INVITE on this view is
/// undecidable, not offending — the vantage has nothing to correlate against.
pub struct RackWithoutKnownInvite;

impl Obligation for RackWithoutKnownInvite {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for p in &seen.pracks {
            let known = seen.invites_opened_by(p.src);
            let head = |decision| Finding {
                rule: RuleId::RackWithoutKnownInvite,
                emitter: p.src.to_string(),
                taker: p.dst.to_string(),
                cseq: p.rack.map_or(0, |(_, cseq)| cseq),
                relayed: seen.relayed_prack(p),
                anchor: p.msg,
                decision,
            };
            let Some((rseq, cseq)) = p.rack else {
                out.push(head(Decision::Undecidable("the PRACK carries no readable INVITE RAck")));
                continue;
            };
            if known.is_empty() {
                out.push(head(Decision::Undecidable(
                    "the emitter opened no INVITE on this view — nothing to correlate",
                )));
            } else if known.contains(&cseq) {
                out.push(head(Decision::Compliant));
            } else {
                out.push(head(Decision::Violated(Evidence::UnknownRack {
                    prack_msg: p.msg,
                    prack_hop: p.hop,
                    prack_ts_us: p.ts_us,
                    rack_rseq: rseq,
                    rack_cseq: cseq,
                    known_cseqs: known,
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §4 — a UAC PRACKs reliable provisionals in `RSeq` order.** A
/// provisional whose `RSeq` is not the next expected arrived out of order (or
/// after one that never came): the UAC buffers it and waits for the gap to
/// fill, because PRACKing it would acknowledge a provisional it has not
/// processed.
///
/// Charges the UAC — the offending message is the PRACK, not the provisional.
/// A PRACK naming an `RSeq` this view never carried to the emitter is
/// undecidable: nothing here says where that provisional sat in the order.
pub struct NoPrackOfOutOfOrderRseq;

impl Obligation for NoPrackOfOutOfOrderRseq {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for p in &seen.pracks {
            let Some((rseq, cseq)) = p.rack else { continue };
            let head = |decision| Finding {
                rule: RuleId::NoPrackOfOutOfOrderRseq,
                emitter: p.src.to_string(),
                taker: p.dst.to_string(),
                cseq,
                relayed: seen.relayed_prack(p),
                anchor: p.msg,
                decision,
            };
            let arrivals: Vec<&Reliable<'_>> = seen
                .taken
                .get(&(p.src, p.dialog))
                .map(|list| list.iter().filter(|r| r.ts_us <= p.ts_us).collect())
                .unwrap_or_default();
            if !arrivals.iter().any(|r| r.rseq == rseq) {
                out.push(head(Decision::Undecidable(
                    "the vantage never carried the provisional this PRACK names",
                )));
                continue;
            }
            let (expected, out_of_order) = rseq_order(&arrivals);
            out.push(head(if out_of_order.contains(&rseq) {
                Decision::Violated(Evidence::OutOfOrderRack {
                    prack_msg: p.msg,
                    prack_hop: p.hop,
                    prack_ts_us: p.ts_us,
                    rack_rseq: rseq,
                    expected_rseq: expected,
                })
            } else {
                Decision::Compliant
            }));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// Replay a dialog's reliable provisionals in arrival order: the `RSeq` owed
/// next, and every `RSeq` that arrived while a different one was owed.
fn rseq_order(arrivals: &[&Reliable<'_>]) -> (u64, BTreeSet<u64>) {
    let mut expected: Option<u64> = None;
    let mut out_of_order = BTreeSet::new();
    for r in arrivals {
        match expected {
            None => expected = Some(r.rseq + 1),
            Some(e) if r.rseq == e => expected = Some(r.rseq + 1),
            Some(e) if r.rseq + 1 == e => {} // a retransmission of the last in order
            Some(_) => {
                out_of_order.insert(r.rseq);
            }
        }
    }
    (expected.unwrap_or_default(), out_of_order)
}

/// **RFC 3262 §4 — a 100 Trying is never PRACKed.** §3 excludes the 100 from
/// the reliable mechanism outright, so `Require: 100rel` and an `RSeq` on one
/// are markers the UAC ignores. PRACKing it acknowledges a provisional that
/// opened no `RSeq` space, and the RAck matches no reliable transaction.
///
/// The occasion is ONE such 100 the endpoint TOOK — the temptation — decided by
/// whether the endpoint went on to PRACK its `RSeq` on the same call. A
/// violated occasion rests on the PRACK, which is the offending message.
/// Charges the UAC.
pub struct NoPrackOf100Trying;

impl Obligation for NoPrackOf100Trying {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for ((uac, call), tryings) in &seen.trying_100rel {
            for t in tryings {
                let offender = seen.pracks.iter().find(|p| {
                    p.src == *uac
                        && p.call_id == *call
                        && p.ts_us > t.ts_us
                        && p.racked_rseq() == Some(t.rseq)
                });
                out.push(Finding {
                    rule: RuleId::NoPrackOf100Trying,
                    emitter: uac.to_string(),
                    taker: offender.map_or(t.peer, |p| p.dst).to_string(),
                    cseq: t.cseq,
                    relayed: false,
                    anchor: offender.map_or(t.msg, |p| p.msg),
                    decision: match offender {
                        None => Decision::Compliant,
                        Some(p) => Decision::Violated(Evidence::PrackedTrying {
                            trying_prack_msg: p.msg,
                            trying_prack_hop: p.hop,
                            trying_prack_ts_us: p.ts_us,
                            rseq: t.rseq,
                            trying_msg: t.msg,
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
