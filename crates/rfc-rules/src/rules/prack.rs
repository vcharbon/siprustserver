//! The RFC 3262 reliable-provisional family: fourteen obligations that share one
//! reading of the wire — who opened the INVITE, which provisionals were sent
//! reliably, and which PRACK named which `RSeq`.
//!
//! **Two halves of one reading.** The OBLIGATION half walks the reliable
//! provisionals and the PRACKs that discharge them, keyed by early dialog and
//! `RSeq`. The SERVER-TRANSACTION half walks what each endpoint TOOK on a
//! top-Via branch and what it ANSWERED there, which is what the §3 negotiation
//! rules weigh: a `Require: 100rel` INVITE against the responses it drew, a
//! PRACK against the status it drew back. Both are collected in one pass so a
//! rule never re-derives a fact a sibling already read.
//!
//! **A hop's exemption is the CONSUMER's, not the rule's, for the §3
//! negotiation rules.** Whether a lane merely relays a stream is a property of
//! that lane across the whole recording, not of the message in hand, so those
//! rules leave `relayed` false and the consumer skips the lanes it knows to be
//! relays — the [`crate::rules::final_response`] precedent.
//!
//! **What makes a provisional reliable** (RFC 3262 §3, and nothing less): the
//! UAC offered `100rel` on the INVITE — in `Require` or in `Supported` — and
//! the UAS answered a 101-199 carrying BOTH `Require: 100rel` and an `RSeq`.
//! A 100 Trying is never sent reliably, an `RSeq` on its own is not the
//! negotiation, and a provisional whose INVITE this vantage never saw is a
//! provisional whose negotiation nothing witnessed.
//!
//! **An obligation belongs to a dialog, not to a socket.** RFC 3262 §3 has the
//! UAS retransmit until a PRACK names its `RSeq`, so an `RSeq` some endpoint
//! on the view acknowledged is an obligation MET, whichever socket spoke: a
//! proxy that does not record-route never sees the PRACK the UAC sent straight
//! past it, and charging that proxy would charge the one box on the wire that
//! owes nothing. The early dialog's To tag is the partition — two forks
//! answering one INVITE number their `RSeq`s independently.
//!
//! **A hop is not a UAC, and a hop is not a UAS.** An endpoint that passed a
//! provisional or a PRACK on relayed the behaviour rather than originating it,
//! and every rule here marks such a finding `relayed` for the consumer to
//! weigh. A B2BUA is untouched: it re-originates under a new Call-ID, so its
//! b-leg INVITE is that leg's first and its obligations there are its own.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::header::{HeaderValue, RAck, Supported};
use sip_message::{sniff, SipStr};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

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
    fn id(&self) -> RuleId {
        RuleId::UnackedReliableProvisional
    }

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
    fn id(&self) -> RuleId {
        RuleId::RackWithoutKnownInvite
    }

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

/// **RFC 3262 §3 — a UAS holds one reliable provisional outstanding at a
/// time.** A second reliable provisional on a dialog waits for the PRACK of
/// the last: the UAS is retransmitting the first on the T1 ladder until then,
/// and two unacknowledged `RSeq`s leave the UAC no way to say which it
/// answered.
///
/// Charges the UAS. The dialog's FIRST provisional is no occasion (nothing can
/// overlap it), and a retransmission of an `RSeq` already sent is not a second
/// provisional.
pub struct NoOverlappingReliableProvisionals;

impl Obligation for NoOverlappingReliableProvisionals {
    fn id(&self) -> RuleId {
        RuleId::NoOverlappingReliableProvisionals
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for ((uas, dialog), list) in &seen.emitted {
            let (uas, dialog) = (*uas, *dialog);
            let mut sent: Vec<&Reliable<'_>> = Vec::new();
            for r in list {
                if sent.iter().any(|q| q.rseq == r.rseq) {
                    continue; // a retransmission answers to the same PRACK
                }
                let outstanding =
                    sent.iter().find(|q| !seen.pracked_before(q, dialog, r.ts_us)).copied();
                if !sent.is_empty() {
                    out.push(Finding {
                        rule: RuleId::NoOverlappingReliableProvisionals,
                        emitter: uas.to_string(),
                        taker: r.peer.to_string(),
                        cseq: r.cseq,
                        relayed: seen.relayed_provisional(uas, dialog, r),
                        anchor: r.msg,
                        decision: match outstanding {
                            Some(prior) => Decision::Violated(Evidence::Overlapping {
                                provisional_msg: r.msg,
                                provisional_hop: r.hop,
                                provisional_ts_us: r.ts_us,
                                rseq: r.rseq,
                                status: r.status,
                                unacked_rseq: prior.rseq,
                            }),
                            None => Decision::Compliant,
                        },
                    });
                }
                sent.push(r);
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — each subsequent reliable provisional carries
/// `RSeq = prior + 1`.** The UAS allocates the dialog's `RSeq` space
/// contiguously, so the UAC can tell a gap (a provisional still in flight)
/// from a re-ordering; `RSeq` never wraps and never moves backwards.
///
/// Charges the UAS. The dialog's FIRST provisional seeds the space and is no
/// occasion; a retransmission carrying the same `RSeq` is not a step. A
/// non-contiguous value re-seeds the reading, so one gap yields one finding
/// rather than charging every provisional after it.
pub struct NonContiguousRseq;

impl Obligation for NonContiguousRseq {
    fn id(&self) -> RuleId {
        RuleId::NonContiguousRseq
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for ((uas, dialog), list) in &seen.emitted {
            let (uas, dialog) = (*uas, *dialog);
            let mut prior: Option<u64> = None;
            for r in list {
                let Some(previous) = prior else {
                    prior = Some(r.rseq);
                    continue;
                };
                if r.rseq == previous {
                    continue; // a retransmission carries the same RSeq
                }
                prior = Some(r.rseq);
                out.push(Finding {
                    rule: RuleId::NonContiguousRseq,
                    emitter: uas.to_string(),
                    taker: r.peer.to_string(),
                    cseq: r.cseq,
                    relayed: seen.relayed_provisional(uas, dialog, r),
                    anchor: r.msg,
                    decision: if r.rseq == previous + 1 {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::RseqGap {
                            provisional_msg: r.msg,
                            provisional_hop: r.hop,
                            provisional_ts_us: r.ts_us,
                            rseq: r.rseq,
                            status: r.status,
                            prior_rseq: previous,
                        })
                    },
                });
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
    fn id(&self) -> RuleId {
        RuleId::NoPrackOfOutOfOrderRseq
    }

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

/// **RFC 3262 §3 — a UAS handed `Require: 100rel` answers reliably, or says it
/// cannot.** The INVITE demanded the extension, so §3 leaves the UAS one
/// choice of two: every non-100 provisional carries `Require: 100rel` AND an
/// `RSeq`, or the INVITE is rejected `420` naming `100rel` in `Unsupported`.
/// A plain 18x against that demand is neither.
///
/// The occasion is ONE provisional the UAS sent on the demanding transaction.
/// The 420 discharges the WHOLE transaction wherever on it it went out — a UAS
/// that rejects the extension owes nothing about the provisionals it already
/// sent. Charges the UAS.
pub struct RequireReliable1xxOnRequire;

impl Obligation for RequireReliable1xxOnRequire {
    fn id(&self) -> RuleId {
        RuleId::RequireReliable1xxOnRequire
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, served) in &seen.served {
            if !served.requires_100rel {
                continue;
            }
            let answers = seen.answers_on(key);
            let rejected = answers.iter().any(|a| a.status == 420 && a.rejects_100rel);
            for a in answers.iter().filter(|a| a.answers("INVITE")) {
                if !(101..200).contains(&a.status) {
                    continue;
                }
                out.push(Finding {
                    rule: RuleId::RequireReliable1xxOnRequire,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false, // a lane that only relays is the consumer's to skip
                    anchor: a.msg,
                    decision: if !a.readable {
                        Decision::Undecidable("the vantage recorded no headers for the response")
                    } else if rejected || (a.requires_100rel && a.rseq.is_some()) {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::Unreliable1xx {
                            unreliable_1xx_msg: a.msg,
                            unreliable_1xx_hop: a.hop,
                            unreliable_1xx_ts_us: a.ts_us,
                            status: a.status,
                            invite_msg: served.msg,
                            branch: key.branch.to_string(),
                        })
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — a reliable provisional needs the client's opt-in.** The
/// PRACK machinery is negotiated, not imposed: without `100rel` in the INVITE's
/// `Require` or `Supported`, a UAS answering reliably obliges a UAC that never
/// said it could PRACK, and the provisional retransmits until the transaction
/// dies.
///
/// The occasion is ONE reliable provisional the UAS sent. An INVITE this
/// vantage never carried leaves it undecidable — the negotiation was witnessed
/// by nothing. Charges the UAS.
pub struct ReliableNeedsClientOptIn;

impl Obligation for ReliableNeedsClientOptIn {
    fn id(&self) -> RuleId {
        RuleId::ReliableNeedsClientOptIn
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list.iter().filter(|a| a.reliable_1xx()) {
                out.push(Finding {
                    rule: RuleId::ReliableNeedsClientOptIn,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: match seen.served.get(key) {
                        None => Decision::Undecidable(
                            "the INVITE this response answers never crossed this vantage",
                        ),
                        Some(invite) if invite.offers_100rel => Decision::Compliant,
                        Some(invite) => Decision::Violated(Evidence::UnsolicitedReliable1xx {
                            unsolicited_1xx_msg: a.msg,
                            unsolicited_1xx_hop: a.hop,
                            unsolicited_1xx_ts_us: a.ts_us,
                            status: a.status,
                            invite_msg: invite.msg,
                            branch: key.branch.to_string(),
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — the PRACK machinery is scoped to the INVITE METHOD, not to
/// the dialog-creating INVITE.** §3 "does not allow reliable provisional
/// responses for any method but INVITE"; §4 bars `Require: 100rel` from every
/// other request and §7 Table 3 permits `RSeq` only in an INVITE response. A
/// re-INVITE is an INVITE: §3's To-tag bar is written for a PROXY and says so
/// ("unlike a UAS"), and RFC 6141 §4.6 has a UAS answer one reliably.
///
/// The occasion is ONE provisional carrying `Require: 100rel` that the UAS sent
/// to a non-INVITE request. A request this vantage never carried leaves it
/// undecidable. Charges the UAS. The serde token is a persisted identity
/// (census, replay verdict registry) and does not track the rule's reading.
pub struct NoReliable1xxOnInDialog;

impl Obligation for NoReliable1xxOnInDialog {
    fn id(&self) -> RuleId {
        RuleId::NoReliable1xxOnInDialog
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list {
                if !((101..200).contains(&a.status) && a.requires_100rel) {
                    continue;
                }
                out.push(Finding {
                    rule: RuleId::NoReliable1xxOnInDialog,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: match seen.served.get(key) {
                        None => Decision::Undecidable(
                            "the request this response answers never crossed this vantage",
                        ),
                        Some(request) if request.method.eq_ignore_ascii_case("INVITE") => {
                            Decision::Compliant
                        }
                        Some(request) => Decision::Violated(Evidence::InDialogReliable1xx {
                            in_dialog_1xx_msg: a.msg,
                            in_dialog_1xx_hop: a.hop,
                            in_dialog_1xx_ts_us: a.ts_us,
                            status: a.status,
                            method: request.method.to_string(),
                            request_msg: request.msg,
                            branch: key.branch.to_string(),
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — a PRACK matching nothing local is forwarded, not
/// absorbed.** A proxy holds no reliable-provisional state, so a PRACK naming
/// an `RSeq` it never carried is one it has no standing to answer: swallowing
/// it strands the UAS retransmitting a provisional whose acknowledgement
/// stopped one hop short.
///
/// The occasion is ONE PRACK the endpoint TOOK, and it is charged to that
/// endpoint. Either half discharges it: an `RSeq` the endpoint had itself taken
/// on a provisional of the call (the PRACK is answerable locally), or the same
/// `RAck` triple going back out (it was forwarded). An unreadable `RAck` names
/// nothing to match against.
pub struct UnmatchedPrackProxied;

impl Obligation for UnmatchedPrackProxied {
    fn id(&self) -> RuleId {
        RuleId::UnmatchedPrackProxied
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for p in &seen.pracks {
            let head = |decision| Finding {
                rule: RuleId::UnmatchedPrackProxied,
                emitter: p.dst.to_string(),
                taker: p.src.to_string(),
                cseq: p.rack_triple.as_ref().map_or(0, |(_, cseq, _)| *cseq),
                relayed: false,
                anchor: p.msg,
                decision,
            };
            let Some(triple) = p.rack_triple.as_ref() else {
                out.push(head(Decision::Undecidable("the PRACK carries no readable RAck")));
                continue;
            };
            let known: Vec<u64> = seen
                .rseqs_taken
                .get(&(p.dst, p.call_id))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            let forwarded = seen.pracks.iter().any(|q| {
                q.src == p.dst && q.call_id == p.call_id && q.rack_triple.as_ref() == Some(triple)
            });
            let (rseq, cseq, method) = triple;
            out.push(head(if known.contains(rseq) || forwarded {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::PrackAbsorbed {
                    absorbed_prack_msg: p.msg,
                    absorbed_prack_hop: p.hop,
                    absorbed_prack_ts_us: p.ts_us,
                    rack_rseq: *rseq,
                    rack_cseq: *cseq,
                    rack_method: method.clone(),
                    known_rseqs: known,
                })
            }));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — a PRACK draws 2xx on a match and 481 without one.** The
/// answer is the UAS's own `RSeq` state read back: a 2xx tells the UAC the
/// provisional is acknowledged and the retransmissions stop; a 481 tells it the
/// transaction it named does not exist here. Any other status leaves the UAC
/// unable to tell which happened.
///
/// The occasion is the FIRST response the endpoint sent on the PRACK's server
/// transaction — a retransmission repeats that answer rather than giving a
/// second. `matched` is the whole §7.2 `RAck` within the early dialog the
/// PRACK names (§5): the method `INVITE`, and the `(RSeq, CSeq-num)` pair of a
/// provisional the UAS had sent reliably in THAT dialog as of the PRACK's
/// ARRIVAL — an `RSeq` it sends afterwards was not yet its to acknowledge, the
/// right `RSeq` under another INVITE's CSeq names nothing, and a sibling
/// fork's number names nothing here (§3, errata 4600). Charges the UAS.
pub struct Prack2xxOr481;

impl Obligation for Prack2xxOr481 {
    fn id(&self) -> RuleId {
        RuleId::Prack2xxOr481
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            let Some(prack) =
                seen.served.get(key).filter(|s| s.method.eq_ignore_ascii_case("PRACK"))
            else {
                continue;
            };
            let Some(a) = list.iter().find(|a| a.answers("PRACK")) else { continue };
            let head = |decision| Finding {
                rule: RuleId::Prack2xxOr481,
                emitter: key.uas.to_string(),
                taker: a.taker.to_string(),
                cseq: a.cseq,
                relayed: false,
                anchor: a.msg,
                decision,
            };
            let Some(rack) = prack.rack else {
                out.push(head(Decision::Undecidable(
                    "the PRACK this response answers carries no readable RAck",
                )));
                continue;
            };
            let matched = rack.on_invite
                && seen
                    .reliably_sent_before(key.uas, key.call_id, prack.dialog, prack.ts_us)
                    .contains(&(rack.rseq, rack.cseq));
            let honoured = if matched { (200..300).contains(&a.status) } else { a.status == 481 };
            out.push(head(if honoured {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::PrackAnsweredWrongly {
                    prack_answer_msg: a.msg,
                    prack_answer_hop: a.hop,
                    prack_answer_ts_us: a.ts_us,
                    status: a.status,
                    answered_prack_msg: prack.msg,
                    rack_rseq: rack.rseq,
                    rack_matched: matched,
                    branch: key.branch.to_string(),
                })
            }));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — the 2xx waits for the PRACK of an offer sent reliably.** A
/// reliable provisional carrying a body put a session description on the wire
/// that the PRACK confirms; answering the INVITE before that confirmation
/// arrives establishes a dialog over a description neither side has agreed.
///
/// The occasion is ONE INVITE 2xx the UAS sent on a transaction where it had
/// already sent a reliable provisional with a body — one finding naming EVERY
/// `RSeq` still outstanding, since the 2xx is the single act judged. Each
/// `RSeq` is judged once, from its first emission: §3 has the UAS repeat the
/// provisional until the PRACK reaches it, so a retransmission crossing the
/// acknowledgement on the wire carries the same `RSeq`, answers to the same
/// PRACK, and re-opens nothing. A body is read off the head (`Content-Length`
/// / `Content-Type`): the presence of a description, which is the
/// conservative proxy for it being an offer. Charges the UAS.
pub struct Delay2xxOnUnackedReliable1xxWithSdp;

impl Obligation for Delay2xxOnUnackedReliable1xxWithSdp {
    fn id(&self) -> RuleId {
        RuleId::Delay2xxOnUnackedReliable1xxWithSdp
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list.iter().filter(|a| a.answers("INVITE") && (200..300).contains(&a.status)) {
                let mut offer_1xx_msg = None;
                let mut unacked: Vec<u64> = Vec::new();
                let mut judged: BTreeSet<u64> = BTreeSet::new();
                // The 2xx confirms ONE early dialog; an abandoned fork's offer
                // is not what it answers, and each fork numbers its own RSeqs.
                for q in list.iter().filter(|q| {
                    q.reliable_1xx() && q.has_body && q.to_tag == a.to_tag && q.ts_us < a.ts_us
                }) {
                    offer_1xx_msg.get_or_insert(q.msg);
                    let Some(rseq) = q.rseq else { continue };
                    // A later copy of an `RSeq` is its retransmission: the
                    // provisional was judged from its first emission.
                    if !judged.insert(rseq) {
                        continue;
                    }
                    let acked = seen.pracked_between(
                        (key.uas, key.call_id, a.to_tag, rseq),
                        q.ts_us,
                        a.ts_us,
                    );
                    if !acked {
                        unacked.push(rseq);
                    }
                }
                // No offer went out reliably on this transaction: the 2xx owed
                // nothing and the obligation was never tested.
                let Some(offer_1xx_msg) = offer_1xx_msg else { continue };
                out.push(Finding {
                    rule: RuleId::Delay2xxOnUnackedReliable1xxWithSdp,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: if unacked.is_empty() {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::AnsweredOverUnackedOffer {
                            early_2xx_msg: a.msg,
                            early_2xx_hop: a.hop,
                            early_2xx_ts_us: a.ts_us,
                            status: a.status,
                            unacked_rseqs: unacked,
                            offer_1xx_msg,
                            branch: key.branch.to_string(),
                        })
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3262 §3 — no NEW reliable provisional after the final.** The final
/// answered the offer and completed the transaction; a fresh `RSeq` after it
/// opens an acknowledgement the UAC has nowhere to put and the UAS will
/// retransmit forever.
///
/// The occasion is ONE reliable provisional carrying an `RSeq` this transaction
/// had not used — a retransmission of an `RSeq` already sent is the required
/// behaviour, not a new provisional. This is the reliable-only sibling of
/// [`crate::rules::final_response::No1xxAfterFinal`], which reads the same
/// emission through the general §17.2.1 lens; the two decide one message
/// against two different obligations. Charges the UAS.
pub struct NoNewReliable1xxAfterFinal;

impl Obligation for NoNewReliable1xxAfterFinal {
    fn id(&self) -> RuleId {
        RuleId::NoNewReliable1xxAfterFinal
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            let mut used: BTreeSet<u64> = BTreeSet::new();
            let mut answered: Option<&Answer<'_>> = None;
            for a in list.iter().filter(|a| a.answers("INVITE")) {
                if a.status >= 200 {
                    answered.get_or_insert(a);
                    continue;
                }
                if !(a.status > 100 && a.requires_100rel) {
                    continue;
                }
                let Some(rseq) = a.rseq else { continue };
                if !used.insert(rseq) {
                    continue; // an RSeq this transaction already used
                }
                out.push(Finding {
                    rule: RuleId::NoNewReliable1xxAfterFinal,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision: match answered {
                        None => Decision::Compliant,
                        Some(prior) => Decision::Violated(Evidence::Reliable1xxAfterFinal {
                            stray_1xx_msg: a.msg,
                            stray_1xx_hop: a.hop,
                            stray_1xx_ts_us: a.ts_us,
                            status: a.status,
                            rseq,
                            prior_final_msg: prior.msg,
                            prior_final_status: prior.status,
                            branch: key.branch.to_string(),
                        }),
                    },
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
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
    fn id(&self) -> RuleId {
        RuleId::NoPrackOf100Trying
    }

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

/// **RFC 3262 §5 — the PRACK of an offer carries the answer.** A reliable
/// provisional may carry the offer; §5 then puts the answer in the PRACK that
/// acknowledges it, because that is the only message guaranteed to arrive
/// before the session is established.
///
/// **A body in a reliable provisional is the OFFER only where the INVITE
/// carried none.** With an offer already in the INVITE the provisional's body
/// is the ANSWER (RFC 3264 §5) and the PRACK owes no body at all — the standard
/// PRACK call setup, which a body-presence reading alone flags on every
/// endpoint of every such flow. An INVITE this vantage never carried keeps the
/// conservative reading (a body is an offer).
///
/// The occasion is ONE PRACK, keyed by the offer's `RSeq` in its early dialog
/// (the To tag, which both directions carry) rather than by direction: the
/// offer and the answer are two ends of one negotiation and either party's
/// copy is the same fact, and each fork numbers its `RSeq`s on its own (§3,
/// errata 4600). Charges the PRACK's sender.
///
/// **This reads BODY PRESENCE, never the offer/answer state machine** — it
/// walks the view directly rather than through the family's `Reading`,
/// because the offer is a fact about the CALL and not about either endpoint's
/// own server transactions.
pub struct PrackAnswers1xxOffer;

impl Obligation for PrackAnswers1xxOffer {
    fn id(&self) -> RuleId {
        RuleId::PrackAnswers1xxOffer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per call: whether an INVITE carried the offer, and where each
        // offer-bearing reliable provisional's `RSeq` was first seen.
        let mut invite_offered: BTreeMap<&str, bool> = BTreeMap::new();
        let mut offers: BTreeMap<(&str, &str, u64), usize> = BTreeMap::new();
        let mut out = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let head = msg.head.as_deref();
            let call = msg.call_id.as_str();
            if msg.is_request("INVITE") {
                let carried = invite_offered.entry(call).or_insert(false);
                *carried = *carried || head.is_some_and(sniff::has_body);
                continue;
            }
            if msg.is_request("PRACK") {
                let Some(rseq) = head.and_then(sniff::rack_rseq) else { continue };
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                let Some(&offer_1xx_msg) = offers.get(&(call, dialog, rseq)) else { continue };
                out.push(Finding {
                    rule: RuleId::PrackAnswers1xxOffer,
                    emitter: msg.src.to_string(),
                    taker: msg.dst.to_string(),
                    cseq: msg.cseq,
                    relayed: false,
                    anchor: mi,
                    decision: if head.is_some_and(sniff::has_body) {
                        Decision::Compliant
                    } else {
                        Decision::Violated(Evidence::PrackWithoutAnswer {
                            bodiless_prack_msg: mi,
                            bodiless_prack_hop: msg.hop,
                            bodiless_prack_ts_us: msg.at_us,
                            rack_rseq: rseq,
                            offer_1xx_msg,
                        })
                    },
                });
                continue;
            }
            if !msg.status().is_some_and(|s| (101..200).contains(&s)) {
                continue;
            }
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                continue;
            }
            let Some(head) = head else { continue };
            if !sniff::require_has_100rel(head) || !sniff::has_body(head) {
                continue;
            }
            if invite_offered.get(call).copied().unwrap_or(false) {
                continue; // the INVITE carried the offer: this body is the answer
            }
            if let Some(rseq) = sniff::rseq_of(head) {
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                offers.entry((call, dialog, rseq)).or_insert(mi);
            }
        }
        out
    }
}

/// One 100rel obligation: the endpoint that owes the PRACK, the transaction
/// and the early dialog it rides, and the `RSeq` the PRACK's RAck must name.
/// Spelled out as a struct so the four parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObligationKey<'a> {
    uac: &'a str,
    /// The Call-ID: one endpoint may carry several calls, each numbering its
    /// CSeqs on its own.
    call: &'a str,
    cseq: u32,
    /// The early dialog's To tag: two forks answering one INVITE number their
    /// RSeqs independently, so the tag is what keeps their obligations apart.
    dialog: &'a str,
    rseq: u64,
}

/// What one view's messages say about the 100rel obligations on it.
#[derive(Debug, Default)]
struct Reading<'a> {
    /// (endpoint, INVITE CSeq number) → the INVITE that endpoint SENT.
    invite: BTreeMap<(&'a str, u32), Invite>,
    /// (endpoint, Call-ID, INVITE CSeq number) → when that endpoint SENT the
    /// INVITE: the per-call reading the obligation's hop test takes.
    opened: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// (Call-ID, From tag, INVITE CSeq number) → the first time that INVITE
    /// crossed any vantage of the view. An endpoint whose own emission is not
    /// that first one relayed an INVITE somebody else opened. The From tag
    /// keeps the two directions of one dialog apart: each side numbers its own
    /// CSeq space (RFC 3261 §12.2.1.1).
    invite_first_us: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// The reliable provisionals taken, one entry per obligation.
    owed: BTreeMap<ObligationKey<'a>, Reliable<'a>>,
    /// (endpoint, To tag) → the reliable provisionals that endpoint SENT on
    /// that early dialog, in emission order.
    emitted: BTreeMap<(&'a str, &'a str), Vec<Reliable<'a>>>,
    /// (endpoint, To tag) → the reliable provisionals that endpoint TOOK on
    /// that early dialog, in arrival order.
    taken: BTreeMap<(&'a str, &'a str), Vec<Reliable<'a>>>,
    /// (INVITE CSeq number, To tag, RSeq) → when some endpoint on this view
    /// first PRACKed it.
    acked_at: BTreeMap<(u32, &'a str, u64), u64>,
    /// Every PRACK the view carried, in observation order.
    pracks: Vec<Prack<'a>>,
    /// (sender, INVITE CSeq number, To tag, RSeq) → the LAST time that
    /// endpoint SENT that provisional. A taker that sent it after taking it
    /// passed it on; a copy it only received again is the responder's
    /// retransmission, which forwards nothing.
    propagated: BTreeMap<(&'a str, u32, &'a str, u64), u64>,
    /// A PRACK this endpoint sent whose `RAck` will not parse: every
    /// obligation of that endpoint on this view becomes undecidable.
    unreadable_prack: BTreeSet<&'a str>,
    /// (UAC, Call-ID, INVITE CSeq number) → when the INVITE's first final
    /// reached the UAC. A CANCEL ends nothing (RFC 3262 §4).
    final_at: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// (UAC, Call-ID, dialog's To tag) → when the responder's BYE on that
    /// dialog first reached the UAC: before the INVITE's final, it ended the
    /// early dialog, and nothing rides it after (RFC 3261 §15.1.2).
    byed_at: BTreeMap<(&'a str, &'a str, &'a str), u64>,
    /// The last timestamp on this view's own stream: after it, an absence is
    /// the observation's rather than the wire's.
    last_ts_us: u64,

    // ── the server-transaction half ────────────────────────────────────────
    /// (UAS, call, branch) → the FIRST request that endpoint took on the
    /// transaction: what every response it sends there answers.
    served: BTreeMap<TxnKey<'a>, Served<'a>>,
    /// (UAS, call, branch) → the responses that endpoint SENT on the
    /// transaction, in emission order.
    answers: BTreeMap<TxnKey<'a>, Vec<Answer<'a>>>,
    /// (endpoint, call) → every `RSeq` that endpoint TOOK on a non-100
    /// provisional, reliable or not: what a PRACK arriving at it can name.
    rseqs_taken: BTreeMap<(&'a str, &'a str), BTreeSet<u64>>,
    /// (endpoint, call) → the `100 Trying`s that endpoint took carrying BOTH
    /// `Require: 100rel` and an `RSeq` — the 100rel §4 has a UAC ignore.
    trying_100rel: BTreeMap<(&'a str, &'a str), Vec<Trying<'a>>>,
    /// (taker, call, early dialog, `RSeq`) → when that endpoint FIRST took a
    /// PRACK naming it. The To tag is in the key for the same reason the
    /// obligation half keys on it: two forks answering one INVITE number their
    /// `RSeq`s independently, so one fork's PRACK settles nothing for the other.
    pracked_at: BTreeMap<(&'a str, &'a str, &'a str, u64), u64>,
}

/// One server transaction as ONE endpoint served it: RFC 3261 §17 names a
/// transaction by its top-Via branch, and the endpoint judged is part of the
/// identity because both ends of a hop see the one branch. Spelled out as a
/// struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    /// The endpoint judged: the UAS side, which takes the request and answers.
    uas: &'a str,
    call_id: &'a str,
    branch: &'a str,
}

/// One request an endpoint TOOK on a server transaction.
#[derive(Debug, Clone, Copy)]
struct Served<'a> {
    msg: usize,
    ts_us: u64,
    /// The request-line method, as the wire spelled it.
    method: &'a str,
    /// It listed `100rel` in `Require`: §3 then obliges reliable provisionals
    /// or a 420.
    requires_100rel: bool,
    /// It listed `100rel` in `Require` OR in `Supported` — the client opt-in a
    /// reliable provisional needs at all.
    offers_100rel: bool,
    /// The early dialog it names — its To tag, empty on a dialog-creating
    /// request. A PRACK acknowledges within the dialog the provisional opened
    /// (RFC 3262 §5), and two forks number their `RSeq`s independently.
    dialog: &'a str,
    /// What its `RAck` names, where the request is a PRACK carrying a readable
    /// one.
    rack: Option<Rack>,
}

/// The three tokens of a PRACK's `RAck` (RFC 3262 §7.2): the response-num, the
/// CSeq-num, and whether the method token is `INVITE` — the only method a
/// reliable provisional answers, so any other names nothing a UAS sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rack {
    rseq: u64,
    cseq: u32,
    on_invite: bool,
}

/// One response an endpoint SENT on a server transaction.
#[derive(Debug, Clone, Copy)]
struct Answer<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    cseq: u32,
    /// Where it went — the other side of any obligation it settles.
    taker: &'a str,
    /// The CSeq method, as the wire spelled it: a PRACK's response and its
    /// INVITE's ride different branches, and a rule states which it judges.
    method: &'a str,
    /// It carried `Require: 100rel` — the reliable-provisional marker.
    requires_100rel: bool,
    /// It named `100rel` in `Unsupported` — what the 420 rejecting the
    /// extension carries (§3's other half of the disjunction).
    rejects_100rel: bool,
    rseq: Option<u64>,
    /// The early dialog it presents — the To tag that keeps two forks
    /// answering one INVITE apart.
    to_tag: &'a str,
    /// Its head declared a body: `Content-Length` above zero, or a
    /// `Content-Type` where no length reads.
    has_body: bool,
    /// The vantage recorded its headers at all — without them nothing here says
    /// which option tags it carried.
    readable: bool,
}

impl Answer<'_> {
    /// A reliable provisional (RFC 3262 §3): a non-100 provisional on an INVITE
    /// transaction carrying `Require: 100rel`.
    fn reliable_1xx(&self) -> bool {
        self.answers("INVITE") && (101..200).contains(&self.status) && self.requires_100rel
    }

    /// It answers a transaction of `method`.
    fn answers(&self, method: &str) -> bool {
        self.method.eq_ignore_ascii_case(method)
    }
}

/// One `100 Trying` an endpoint took carrying the reliable-provisional markers
/// a 100 may never carry.
#[derive(Debug, Clone, Copy)]
struct Trying<'a> {
    msg: usize,
    ts_us: u64,
    cseq: u32,
    rseq: u64,
    /// Who sent it — the other side of the occasion.
    peer: &'a str,
}

/// One INVITE as this view carried it.
#[derive(Debug, Clone, Copy)]
struct Invite {
    /// It named `100rel` in `Require` or in `Supported`.
    offers_100rel: bool,
}

/// One reliable provisional as this view carried it.
#[derive(Debug, Clone, Copy)]
struct Reliable<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    /// The INVITE CSeq number whose `RSeq` space this provisional indexes.
    cseq: u32,
    rseq: u64,
    /// The endpoint on the other side of the obligation: for one the UAC took,
    /// the UAS that owes the retransmissions; for one the UAS sent, the UAC
    /// that owes the PRACK.
    peer: &'a str,
}

/// One PRACK as this view carried it.
#[derive(Debug, Clone)]
struct Prack<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    src: &'a str,
    dst: &'a str,
    /// The call it rides — a view carries many.
    call_id: &'a str,
    /// The early dialog's To tag.
    dialog: &'a str,
    /// The `(RSeq, INVITE CSeq number)` its `RAck` names, or `None` where no
    /// reader accepts the header.
    rack: Option<(u64, u32)>,
    /// The FULL `RAck` triple `(response-num, CSeq-num, method)`, the method
    /// ASCII-uppercased — what identifies one PRACK as the copy of another,
    /// whatever method the header names.
    rack_triple: Option<(u64, u32, String)>,
}

impl Prack<'_> {
    /// The `RSeq` its `RAck` acknowledges, whatever method the header names.
    fn racked_rseq(&self) -> Option<u64> {
        self.rack_triple.as_ref().map(|(rseq, _, _)| *rseq)
    }
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.last_ts_us = seen.last_ts_us.max(msg.at_us);
            // A repeat is not a fresh ACT, but it is still a fact on the wire
            // ([`crate::wire`]): it RESTATES the request a transaction serves,
            // and a PRACK that is a repeat still MEETS what it acknowledges.
            // Only the occasion ledgers below skip it.
            seen.restated(mi, msg);
            if msg.repeat {
                continue;
            }
            seen.absorb(mi, msg);
        }
        seen
    }

    /// The facts a retransmission restates rather than re-enacts: the request
    /// each endpoint serves on a branch, and the PRACKs it has taken.
    ///
    /// Taking repeats here is load-bearing, not tidiness: where a vantage drops
    /// the first copy of a message and keeps the retransmission, that copy is
    /// marked a repeat and is the ONLY statement of the fact the view carries.
    fn restated(&mut self, mi: usize, msg: &'a Msg) {
        let Kind::Request { method } = &msg.kind else { return };
        let head = msg.head.as_deref();
        let call = msg.call_id.as_str();
        if let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) {
            self.served.entry(TxnKey { uas: msg.dst.as_str(), call_id: call, branch }).or_insert(
                Served {
                    msg: mi,
                    ts_us: msg.at_us,
                    method: method.as_str(),
                    requires_100rel: head.is_some_and(sniff::require_has_100rel),
                    offers_100rel: head.is_some_and(offers_100rel),
                    dialog: msg.to_tag.as_deref().unwrap_or_default(),
                    rack: head.and_then(rack_triple).map(|(rseq, cseq, method)| Rack {
                        rseq,
                        cseq,
                        on_invite: method == "INVITE",
                    }),
                },
            );
        }
        if method.eq_ignore_ascii_case("PRACK") {
            if let Some(rseq) = head.and_then(sniff::rack_rseq) {
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                let at = self
                    .pracked_at
                    .entry((msg.dst.as_str(), call, dialog, rseq))
                    .or_insert(msg.at_us);
                *at = (*at).min(msg.at_us);
            }
        }
    }

    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        let head = msg.head.as_deref();
        self.absorb_transaction(mi, msg, head);
        if msg.is_request("INVITE") {
            let call = msg.call_id.as_str();
            let from_tag = msg.from_tag.as_deref().unwrap_or_default();
            let first = self.invite_first_us.entry((call, from_tag, msg.cseq)).or_insert(msg.at_us);
            *first = (*first).min(msg.at_us);
            self.opened.entry((msg.src.as_str(), call, msg.cseq)).or_insert(msg.at_us);
            self.invite
                .entry((msg.src.as_str(), msg.cseq))
                .or_insert(Invite { offers_100rel: head.is_some_and(offers_100rel) });
        } else if msg.is_request("BYE") {
            let dialog = msg.from_tag.as_deref().unwrap_or_default();
            let at = self
                .byed_at
                .entry((msg.dst.as_str(), msg.call_id.as_str(), dialog))
                .or_insert(msg.at_us);
            *at = (*at).min(msg.at_us);
        } else if msg.is_request("PRACK") {
            let rack = head.and_then(rack_of);
            let dialog = msg.to_tag.as_deref().unwrap_or_default();
            match rack {
                Some((rseq, seq)) => {
                    self.acked_at.entry((seq, dialog, rseq)).or_insert(msg.at_us);
                }
                None => {
                    self.unreadable_prack.insert(msg.src.as_str());
                }
            }
            self.pracks.push(Prack {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                src: msg.src.as_str(),
                dst: msg.dst.as_str(),
                call_id: msg.call_id.as_str(),
                dialog,
                rack,
                rack_triple: head.and_then(rack_triple),
            });
        } else if let Some(status) = msg.status() {
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            if status >= 200 {
                self.answered_finally(msg.dst.as_str(), msg.call_id.as_str(), msg.cseq, msg.at_us);
                return;
            }
            let Some(head) = head else { return };
            // §3: 100 is never reliable, and the marker is BOTH headers.
            if status < 101 || !sniff::require_has_100rel(head) {
                return;
            }
            let Some(rseq) = sniff::rseq_of(head) else { return };
            let dialog = msg.to_tag.as_deref().unwrap_or_default();
            let last = self
                .propagated
                .entry((msg.src.as_str(), msg.cseq, dialog, rseq))
                .or_insert(msg.at_us);
            *last = (*last).max(msg.at_us);
            let common = Reliable {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
                cseq: msg.cseq,
                rseq,
                peer: msg.src.as_str(),
            };
            self.owed
                .entry(ObligationKey {
                    uac: msg.dst.as_str(),
                    call: msg.call_id.as_str(),
                    cseq: msg.cseq,
                    dialog,
                    rseq,
                })
                .or_insert(common);
            self.taken.entry((msg.dst.as_str(), dialog)).or_default().push(common);
            self.emitted
                .entry((msg.src.as_str(), dialog))
                .or_default()
                .push(Reliable { peer: msg.dst.as_str(), ..common });
        }
    }

    /// The occasion half of the server-transaction walk: the responses each
    /// endpoint ANSWERED with on a branch, and the per-call `RSeq` facts a
    /// PRACK is weighed against. Repeats never reach it — a retransmission is
    /// not a second answer — and the facts a repeat restates are
    /// [`Reading::restated`]'s.
    ///
    /// **A message with no top-Via branch keys no transaction.** The branch IS
    /// the pairing; without one, no response can be matched to the request it
    /// answers, and the reading states nothing rather than guessing.
    fn absorb_transaction(&mut self, mi: usize, msg: &'a Msg, head: Option<&[u8]>) {
        let call = msg.call_id.as_str();
        let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
        match &msg.kind {
            Kind::Request { .. } => {}
            Kind::Response { status } => {
                let status = *status;
                if let Some(branch) = branch {
                    self.answers
                        .entry(TxnKey { uas: msg.src.as_str(), call_id: call, branch })
                        .or_default()
                        .push(Answer {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            status,
                            cseq: msg.cseq,
                            taker: msg.dst.as_str(),
                            method: msg.cseq_method.as_str(),
                            requires_100rel: head.is_some_and(sniff::require_has_100rel),
                            rejects_100rel: head
                                .is_some_and(|h| lists_option(h, "Unsupported", "100rel")),
                            rseq: head.and_then(sniff::rseq_of),
                            to_tag: msg.to_tag.as_deref().unwrap_or_default(),
                            has_body: head.is_some_and(sniff::has_body),
                            readable: head.is_some(),
                        });
                }
                let Some(rseq) = head.and_then(sniff::rseq_of) else { return };
                if status == 100 {
                    // §4: a 100 is never reliable, so an RSeq on one is a
                    // temptation rather than an obligation.
                    if head.is_some_and(sniff::require_has_100rel) {
                        self.trying_100rel.entry((msg.dst.as_str(), call)).or_default().push(
                            Trying {
                                msg: mi,
                                ts_us: msg.at_us,
                                cseq: msg.cseq,
                                rseq,
                                peer: msg.src.as_str(),
                            },
                        );
                    }
                } else if (101..200).contains(&status) {
                    self.rseqs_taken.entry((msg.dst.as_str(), call)).or_default().insert(rseq);
                }
            }
        }
    }

    /// The responses one endpoint sent on one server transaction, in emission
    /// order; empty where this vantage carried none.
    fn answers_on(&self, key: &TxnKey<'a>) -> &[Answer<'a>] {
        self.answers.get(key).map_or(&[][..], Vec::as_slice)
    }

    /// The `(RSeq, INVITE CSeq-num)` pairs `uas` had already sent reliably on
    /// the `dialog` early dialog of `call` before `ts_us` — what a PRACK
    /// arriving then in that dialog could legitimately name (§3, §7.2). The To
    /// tag is in the key for the same reason [`Self::pracked_at`]'s is: two
    /// forks answering one INVITE number their `RSeq`s independently, so one
    /// fork's number names nothing in the other's dialog.
    fn reliably_sent_before(
        &self,
        uas: &str,
        call: &str,
        dialog: &str,
        ts_us: u64,
    ) -> BTreeSet<(u64, u32)> {
        self.answers
            .iter()
            .filter(|(k, _)| k.uas == uas && k.call_id == call)
            .flat_map(|(_, list)| list.iter())
            .filter(|a| a.ts_us < ts_us && a.reliable_1xx() && a.to_tag == dialog)
            .filter_map(|a| a.rseq.map(|rseq| (rseq, a.cseq)))
            .collect()
    }

    /// Whether `uas` took a PRACK naming `rseq` on that early dialog of `call`
    /// between the provisional that owed it and a later emission of its own.
    fn pracked_between(&self, key: (&str, &str, &str, u64), after: u64, before: u64) -> bool {
        self.pracked_at.get(&key).is_some_and(|at| *at > after && *at < before)
    }

    fn answered_finally(&mut self, uac: &'a str, call: &'a str, cseq: u32, ts_us: u64) {
        let at = self.final_at.entry((uac, call, cseq)).or_insert(ts_us);
        *at = (*at).min(ts_us);
    }

    /// The INVITE CSeq numbers `endpoint` opened on this view.
    fn invites_opened_by(&self, endpoint: &str) -> Vec<u32> {
        self.invite.keys().filter(|(ep, _)| *ep == endpoint).map(|(_, cseq)| *cseq).collect()
    }

    /// Whether `r` was PRACKed before `ts_us`.
    fn pracked_before(&self, r: &Reliable<'a>, dialog: &'a str, ts_us: u64) -> bool {
        self.acked_at.get(&(r.cseq, dialog, r.rseq)).is_some_and(|at| *at < ts_us)
    }

    /// The endpoint passed this provisional on rather than originating it: the
    /// same `RSeq` reached it earlier on this view.
    fn relayed_provisional(&self, ep: &'a str, dialog: &'a str, r: &Reliable<'a>) -> bool {
        self.taken.get(&(ep, dialog)).is_some_and(|list| {
            list.iter().any(|q| q.cseq == r.cseq && q.rseq == r.rseq && q.ts_us < r.ts_us)
        })
    }

    /// The endpoint passed this PRACK on rather than originating it: the same
    /// `RAck` reached it earlier on this view.
    fn relayed_prack(&self, p: &Prack<'a>) -> bool {
        p.rack.is_some_and(|rack| {
            self.pracks.iter().any(|q| q.dst == p.src && q.rack == Some(rack) && q.ts_us < p.ts_us)
        })
    }
}

/// Whether an INVITE offers `100rel`, in `Require` (RFC 3262 §3: the UAS MUST
/// then send reliably) or in `Supported` (it MAY).
fn offers_100rel(raw: &[u8]) -> bool {
    if sniff::require_has_100rel(raw) {
        return true;
    }
    ["Supported", "k"].iter().any(|name| {
        sniff::header_value(raw, name).is_some_and(|v| {
            Supported::parse(&SipStr::owned(&v)).is_ok_and(|s| s.contains("100rel"))
        })
    })
}

/// Whether the set-like header `name` lists `tag` (RFC 3261 §7.3.1: option
/// tags are case-insensitive, comma folds and repeated rows unioned).
fn lists_option(raw: &[u8], name: &str, tag: &str) -> bool {
    sniff::option_tags(raw, name).iter().any(|t| t.eq_ignore_ascii_case(tag))
}

/// The FULL `(response-num, CSeq-num, method)` a PRACK's `RAck` names
/// (RFC 3262 §7.2), the method ASCII-uppercased. Unlike [`rack_of`] it accepts
/// any method: what identifies one PRACK as the copy of another is the triple
/// its sender wrote, not the triple it should have written.
fn rack_triple(raw: &[u8]) -> Option<(u64, u32, String)> {
    let value = sniff::header_value(raw, "RAck")?;
    let rack = RAck::parse(&SipStr::owned(&value)).ok()?;
    Some((u64::from(rack.rseq()), rack.seq(), rack.method().as_str().to_ascii_uppercase()))
}

/// The `(RSeq, CSeq number)` a PRACK's `RAck` names, for an INVITE
/// (RFC 3262 §7.2). `None` for an absent, unparseable or non-INVITE `RAck` —
/// every one of which leaves the obligation undecidable rather than unmet.
fn rack_of(raw: &[u8]) -> Option<(u64, u32)> {
    let value = sniff::header_value(raw, "RAck")?;
    let rack = RAck::parse(&SipStr::owned(&value)).ok()?;
    rack.method()
        .as_str()
        .eq_ignore_ascii_case("INVITE")
        .then_some((u64::from(rack.rseq()), rack.seq()))
}

#[cfg(test)]
mod tests;
