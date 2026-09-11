//! The RFC 3262 reliable-provisional family: fifteen obligations that share one
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

/// How long a UAC must be seen holding an unPRACKed reliable provisional in an
/// OPEN observation before the absence is charged to it.
///
/// One second: RFC 3262 §3 has the UAS retransmit the provisional on the T1
/// ladder (500 ms, then 1 s, …) until the PRACK arrives, and §4 has the UAC
/// PRACK on receipt. A UAC that has neither PRACKed nor been released a full
/// second later is not slow, and anything shorter would charge a dialog whose
/// PRACK was still in flight.
pub const PRACK_WINDOW_US: u64 = 1_000_000;

/// **RFC 3262 §4 — a UAC that took a reliable provisional answers it with a
/// PRACK whose `RAck` names it.**
///
/// The offence is an ABSENCE; each conservatism gate costs an occasion the
/// population counts as undecided rather than clean: the negotiation must be
/// witnessed, the charged endpoint must be the transaction's UAC, a PRACK
/// whose `RAck` will not parse contaminates its sender's obligations, and the
/// dialog must stay observably alive for [`PRACK_WINDOW_US`] after the
/// provisional.
///
/// **A CLOSED observation DECIDES** (issue 29 D2): end-of-stream is
/// end-of-world, so an unPRACKed reliable provisional is Violated even where a
/// final response followed it milliseconds later — nothing was in flight when
/// the observation stopped. The released-inside-the-window gate below is an
/// OPEN observation's conservatism only.
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
            let ObligationKey { uac, cseq, dialog, rseq } = *key;
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
            let alive_until = seen
                .released
                .get(&(uac, cseq))
                .copied()
                .unwrap_or(seen.last_ts_us)
                .min(seen.last_ts_us);
            let window_us = alive_until.saturating_sub(p.ts_us);
            // The two hop tests, each the other's blind spot: an INVITE this
            // endpoint did not open, and a provisional it passed on.
            let opened_it = seen
                .invite
                .get(&(uac, cseq))
                .is_some_and(|i| seen.invite_first_us.get(&cseq) == Some(&i.at_us));
            let forwarded_it =
                seen.propagated.get(&(cseq, dialog, rseq)).is_some_and(|last| p.ts_us < *last);
            let undecidable = if !opened_it {
                Some("the charged endpoint did not open the INVITE (a hop is not a UAC)")
            } else if forwarded_it {
                Some("the endpoint passed the provisional on (a hop is not a UAC)")
            } else if !seen.invite.get(&(uac, cseq)).is_some_and(|i| i.offers_100rel) {
                Some("the INVITE did not offer 100rel — the negotiation was never witnessed")
            } else if seen.unreadable_prack.contains(uac) {
                Some("a PRACK the emitter sent has an unreadable RAck — it may be the one")
            } else if !(wire.obs.closed || window_us >= PRACK_WINDOW_US) {
                Some("the dialog was released inside the window — a PRACK may have crossed it")
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

/// **RFC 3262 §3 — a late PRACK is still answered 2xx.** The PRACK's own server
/// transaction is not the INVITE's and does not end with it, so a PRACK that
/// crossed the final response still names a provisional the UAS really sent.
/// Rejecting it leaves the UAC believing its acknowledgement never landed.
///
/// The occasion is the FIRST response the endpoint sent on the transaction of a
/// PRACK it took AFTER its own INVITE final. A PRACK that arrived while the
/// INVITE was still open is [`Prack2xxOr481`]'s occasion, never this one's.
/// Charges the UAS.
pub struct PrackAcceptedAfterFinal;

impl Obligation for PrackAcceptedAfterFinal {
    fn id(&self) -> RuleId {
        RuleId::PrackAcceptedAfterFinal
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
            let Some(prior) = seen.invite_final_before(key.uas, key.call_id, prack.ts_us) else {
                continue;
            };
            let Some(a) = list.iter().find(|a| a.answers("PRACK")) else { continue };
            out.push(Finding {
                rule: RuleId::PrackAcceptedAfterFinal,
                emitter: key.uas.to_string(),
                taker: a.taker.to_string(),
                cseq: a.cseq,
                relayed: false,
                anchor: a.msg,
                decision: if (200..300).contains(&a.status) {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::LatePrackRejected {
                        late_prack_answer_msg: a.msg,
                        late_prack_answer_hop: a.hop,
                        late_prack_answer_ts_us: a.ts_us,
                        status: a.status,
                        late_prack_msg: prack.msg,
                        prior_final_msg: prior.msg,
                        prior_final_status: prior.status,
                        branch: key.branch.to_string(),
                    })
                },
            });
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
/// The occasion is ONE PRACK, keyed by the offer's `RSeq` on its CALL rather
/// than by direction: the offer and the answer are two ends of one negotiation
/// and either party's copy is the same fact. Charges the PRACK's sender.
///
/// **This reads BODY PRESENCE, never the offer/answer state machine** — it
/// walks the view directly rather than through the family's [`Reading`],
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
        let mut offers: BTreeMap<(&str, u64), usize> = BTreeMap::new();
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
                let Some(&offer_1xx_msg) = offers.get(&(call, rseq)) else { continue };
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
                offers.entry((call, rseq)).or_insert(mi);
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
    /// INVITE CSeq number → the first time that INVITE crossed any vantage of
    /// the view. An endpoint whose own emission is not that first one relayed
    /// an INVITE somebody else opened.
    invite_first_us: BTreeMap<u32, u64>,
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
    /// (INVITE CSeq number, To tag, RSeq) → the LAST time that provisional
    /// crossed any vantage of the view. A taker that saw it before then passed
    /// it on.
    propagated: BTreeMap<(u32, &'a str, u64), u64>,
    /// A PRACK this endpoint sent whose `RAck` will not parse: every
    /// obligation of that endpoint on this view becomes undecidable.
    unreadable_prack: BTreeSet<&'a str>,
    /// (UAC, INVITE CSeq number) → when the transaction was released, whether
    /// by a final response reaching the UAC or by the UAC cancelling it.
    released: BTreeMap<(&'a str, u32), u64>,
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
    at_us: u64,
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
            let first = self.invite_first_us.entry(msg.cseq).or_insert(msg.at_us);
            *first = (*first).min(msg.at_us);
            self.invite.entry((msg.src.as_str(), msg.cseq)).or_insert(Invite {
                offers_100rel: head.is_some_and(offers_100rel),
                at_us: msg.at_us,
            });
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
        } else if msg.is_request("CANCEL") {
            // The UAC gave up on the transaction: nothing it took before that
            // is charged, because a PRACK may have crossed the CANCEL.
            self.release(msg.src.as_str(), msg.cseq, msg.at_us);
        } else if let Some(status) = msg.status() {
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            if status >= 200 {
                self.release(msg.dst.as_str(), msg.cseq, msg.at_us);
                return;
            }
            let Some(head) = head else { return };
            // §3: 100 is never reliable, and the marker is BOTH headers.
            if status < 101 || !sniff::require_has_100rel(head) {
                return;
            }
            let Some(rseq) = sniff::rseq_of(head) else { return };
            let dialog = msg.to_tag.as_deref().unwrap_or_default();
            let last = self.propagated.entry((msg.cseq, dialog, rseq)).or_insert(msg.at_us);
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
                .entry(ObligationKey { uac: msg.dst.as_str(), cseq: msg.cseq, dialog, rseq })
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

    /// The FIRST INVITE final `uas` sent on `call` before `ts_us`: what makes a
    /// PRACK arriving then a LATE one (§3).
    fn invite_final_before(&self, uas: &str, call: &str, ts_us: u64) -> Option<&Answer<'a>> {
        self.answers
            .iter()
            .filter(|(k, _)| k.uas == uas && k.call_id == call)
            .flat_map(|(_, list)| list.iter())
            .filter(|a| a.ts_us < ts_us && a.status >= 200 && a.answers("INVITE"))
            .min_by_key(|a| a.ts_us)
    }

    /// Whether `uas` took a PRACK naming `rseq` on that early dialog of `call`
    /// between the provisional that owed it and a later emission of its own.
    fn pracked_between(&self, key: (&str, &str, &str, u64), after: u64, before: u64) -> bool {
        self.pracked_at.get(&key).is_some_and(|at| *at > after && *at < before)
    }

    fn release(&mut self, uac: &'a str, cseq: u32, ts_us: u64) {
        let at = self.released.entry((uac, cseq)).or_insert(ts_us);
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
mod tests {
    //! The family's OWN semantics: what an adapter's event fixtures cannot
    //! state — the closed-observation ruling (D2), the fork partition, and
    //! each rule's undecidable gates.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        Delay2xxOnUnackedReliable1xxWithSdp, NoNewReliable1xxAfterFinal,
        NoOverlappingReliableProvisionals, NoPrackOf100Trying, NoPrackOfOutOfOrderRseq,
        NoReliable1xxOnInDialog, NonContiguousRseq, Prack2xxOr481, PrackAcceptedAfterFinal,
        PrackAnswers1xxOffer, RackWithoutKnownInvite, ReliableNeedsClientOptIn,
        RequireReliable1xxOnRequire, UnackedReliableProvisional, UnmatchedPrackProxied,
    };

    const UAC: &str = "10.0.0.1:5060";
    const UAS: &str = "10.0.0.2:5060";

    fn msg(
        at_us: u64,
        src: &str,
        dst: &str,
        kind: Kind,
        cseq: u32,
        to_tag: Option<&str>,
        head: String,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind,
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            via_branch: None,
            from_tag: Some("fa".to_string()),
            to_tag: to_tag.map(str::to_string),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// An INVITE offering `100rel`, from `src`.
    fn invite(at_us: u64, src: &str, dst: &str, cseq: u32) -> Msg {
        let head = format!(
            "INVITE sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>\r\n\
             CSeq: {cseq} INVITE\r\nSupported: 100rel\r\nContent-Length: 0\r\n\r\n"
        );
        msg(at_us, src, dst, Kind::Request { method: "INVITE".to_string() }, cseq, None, head)
    }

    /// A reliable provisional the UAS sends on early dialog `to_tag`.
    fn reliable(
        at_us: u64,
        src: &str,
        dst: &str,
        status: u16,
        cseq: u32,
        rseq: u64,
        to_tag: &str,
    ) -> Msg {
        let head = format!(
            "SIP/2.0 {status} Ringing\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nRequire: 100rel\r\nRSeq: {rseq}\r\nContent-Length: 0\r\n\r\n"
        );
        msg(at_us, src, dst, Kind::Response { status }, cseq, Some(to_tag), head)
    }

    /// A PRACK whose `RAck` names `(rseq, rack_cseq)`.
    fn prack(
        at_us: u64,
        src: &str,
        dst: &str,
        cseq: u32,
        rseq: u64,
        rack_cseq: u32,
        to_tag: &str,
    ) -> Msg {
        let head = format!(
            "PRACK sip:bob@h SIP/2.0\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} PRACK\r\nRAck: {rseq} {rack_cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        let mut m = msg(
            at_us,
            src,
            dst,
            Kind::Request { method: "PRACK".to_string() },
            cseq,
            Some(to_tag),
            head,
        );
        m.cseq_method = "PRACK".to_string();
        m
    }

    /// The INVITE final that releases the transaction.
    fn ok200(at_us: u64, cseq: u32, to_tag: &str) -> Msg {
        let head = format!(
            "SIP/2.0 200 OK\r\nFrom: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\n\
             CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        msg(at_us, UAS, UAC, Kind::Response { status: 200 }, cseq, Some(to_tag), head)
    }

    fn obs(msgs: &[Msg], closed: bool) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed }
    }

    /// **D2, ruled.** A reliable 180 followed 20 ms later by the 200 OK: an
    /// OPEN observation cannot charge it (the release may have crossed the
    /// PRACK), a CLOSED one decides — the harness drained, so nothing was in
    /// flight and the PRACK never existed.
    #[test]
    fn a_closed_observation_decides_a_provisional_a_fast_final_released() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
            ok200(1_120_000, 1, "tb"),
        ];
        let open = obs(&msgs, false);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &open });
        assert_eq!(f.len(), 1);
        assert!(
            matches!(f[0].decision, Decision::Undecidable(_)),
            "open: the release may have crossed the PRACK: {:?}",
            f[0].decision
        );

        let closed = obs(&msgs, true);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &closed });
        assert!(f[0].violated(), "closed: nothing was in flight: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the UAC owed the PRACK");
        assert_eq!(f[0].rule, RuleId::UnackedReliableProvisional);
    }

    /// The PRACK discharges the obligation whatever else follows.
    #[test]
    fn a_pracked_provisional_is_compliant() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
            prack(1_150_000, UAC, UAS, 2, 5, 1, "tb"),
            ok200(1_200_000, 1, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = UnackedReliableProvisional.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// `rack-without-known-invite`: the RAck CSeq-num is copied from the
    /// acknowledged provisional, so a PRACK naming its own CSeq charges its
    /// sender and the finding names the INVITE it should have quoted.
    #[test]
    fn a_rack_naming_the_pracks_own_cseq_charges_its_sender() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 183, 1, 12_800_221, "tb"),
            prack(1_150_000, UAC, UAS, 101, 12_800_221, 101, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = RackWithoutKnownInvite.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAC, "the PRACK's sender is at fault");
        let Decision::Violated(Evidence::UnknownRack { rack_cseq, known_cseqs, .. }) =
            &f[0].decision
        else {
            panic!("rack evidence: {:?}", f[0].decision)
        };
        assert_eq!(*rack_cseq, 101);
        assert_eq!(known_cseqs, &vec![1], "the INVITE the sender did open");
    }

    /// A PRACK whose RAck names the INVITE its sender opened is compliant; one
    /// sent by an endpoint that opened no INVITE here is undecidable, never a
    /// hit — the vantage has nothing to correlate against.
    #[test]
    fn a_rack_is_compliant_on_a_known_invite_and_undecidable_with_none() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 7),
            reliable(1_100_000, UAS, UAC, 180, 7, 5, "tb"),
            prack(1_150_000, UAC, UAS, 2, 5, 7, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = RackWithoutKnownInvite.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        let orphan = vec![prack(1_150_000, UAC, UAS, 2, 5, 7, "tb")];
        let closed = obs(&orphan, true);
        let f = RackWithoutKnownInvite.eval(&WireView { msgs: &orphan, obs: &closed });
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
    }

    /// `no-overlapping-reliable-provisionals`: the second provisional waits for
    /// the first one's PRACK — and a retransmission of an RSeq already sent is
    /// no occasion at all.
    #[test]
    fn a_second_provisional_before_the_prack_charges_the_uas() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
            reliable(1_150_000, UAS, UAC, 180, 1, 5, "tb"), // retransmission
            reliable(1_200_000, UAS, UAC, 183, 1, 6, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = NoOverlappingReliableProvisionals.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1, "one occasion: the 183, not the retransmit: {f:?}");
        assert_eq!(f[0].emitter, UAS);
        assert_eq!(f[0].anchor, 3);
        let Decision::Violated(Evidence::Overlapping { unacked_rseq, rseq, .. }) = &f[0].decision
        else {
            panic!("overlap evidence: {:?}", f[0].decision)
        };
        assert_eq!((*unacked_rseq, *rseq), (5, 6));
    }

    /// The PRACK of the first provisional releases the UAS to send the next.
    #[test]
    fn a_second_provisional_after_the_prack_is_compliant() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
            prack(1_150_000, UAC, UAS, 2, 5, 1, "tb"),
            reliable(1_200_000, UAS, UAC, 183, 1, 6, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = NoOverlappingReliableProvisionals.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// `non-contiguous-rseq`: a gap charges the UAS at the offending
    /// provisional, and each fork numbers its own early dialog — two forks
    /// starting at the same RSeq is not a step backwards.
    #[test]
    fn an_rseq_gap_charges_the_uas_and_forks_number_independently() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 5, "tb"),
            reliable(1_200_000, UAS, UAC, 183, 1, 8, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = NonContiguousRseq.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 2, "the offending message is the 183");
        let Decision::Violated(Evidence::RseqGap { prior_rseq, rseq, .. }) = &f[0].decision else {
            panic!("gap evidence: {:?}", f[0].decision)
        };
        assert_eq!((*prior_rseq, *rseq), (5, 8));

        let forked = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 183, 1, 1, "f1"),
            reliable(1_120_000, UAS, UAC, 183, 1, 1, "f2"),
        ];
        let closed = obs(&forked, true);
        assert!(
            NonContiguousRseq.eval(&WireView { msgs: &forked, obs: &closed }).is_empty(),
            "each fork's first provisional seeds its own RSeq space"
        );
    }

    /// `no-prack-of-out-of-order-rseq`: the offending message is the PRACK the
    /// UAC sent for a provisional that jumped the gap; an in-order PRACK on the
    /// same dialog stays compliant.
    #[test]
    fn pracking_an_out_of_order_provisional_charges_the_uac_at_the_prack() {
        let msgs = vec![
            invite(1_000_000, UAC, UAS, 1),
            reliable(1_100_000, UAS, UAC, 180, 1, 1, "tb"),
            prack(1_150_000, UAC, UAS, 2, 1, 1, "tb"),
            reliable(1_200_000, UAS, UAC, 183, 1, 5, "tb"),
            prack(1_250_000, UAC, UAS, 3, 5, 1, "tb"),
        ];
        let closed = obs(&msgs, true);
        let f = NoPrackOfOutOfOrderRseq.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "in order: {:?}", f[0].decision);
        assert_eq!(f[1].anchor, 4, "the offending message is the second PRACK");
        assert_eq!(f[1].emitter, UAC);
        let Decision::Violated(Evidence::OutOfOrderRack { rack_rseq, expected_rseq, .. }) =
            &f[1].decision
        else {
            panic!("order evidence: {:?}", f[1].decision)
        };
        assert_eq!((*rack_rseq, *expected_rseq), (5, 2));
    }

    /// A PRACK naming an RSeq no provisional on this view carried to its sender
    /// says nothing about order — undecidable, never a hit.
    #[test]
    fn a_prack_for_an_unseen_rseq_is_undecidable_on_order() {
        let msgs = vec![invite(1_000_000, UAC, UAS, 1), prack(1_150_000, UAC, UAS, 2, 9, 1, "tb")];
        let closed = obs(&msgs, true);
        let f = NoPrackOfOutOfOrderRseq.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
    }

    // ── the §3 negotiation half ─────────────────────────────────────────────
    //
    // These rules pair a request an endpoint TOOK on a top-Via branch with the
    // responses it SENT there, so their fixtures carry a branch (the ones above
    // deliberately do not — the obligation half never keys on one).

    const INV_BRANCH: &str = "z9hG4bK-i";
    const PRACK_BRANCH: &str = "z9hG4bK-p";
    const UPDATE_BRANCH: &str = "z9hG4bK-u";
    const SDP: &str = "v=0\r\no=- 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\nm=audio 5004 RTP/AVP 0\r\n";

    /// The body rows a message carries, as a rule reads them off the head.
    fn body_rows(body: bool) -> String {
        match body {
            true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
            false => "Content-Length: 0\r\n".to_string(),
        }
    }

    /// The reliable-provisional markers §3 requires of BOTH halves.
    fn reliable_rows(rseq: u64) -> String {
        format!("Require: 100rel\r\nRSeq: {rseq}\r\n")
    }

    /// An INVITE the UAS takes, with caller-chosen option-tag rows, To tag and
    /// body.
    fn invite_on(at_us: u64, extra: &str, to_tag: Option<&str>, body: bool) -> Msg {
        let to = match to_tag {
            Some(t) => format!("<sip:b@h>;tag={t}"),
            None => "<sip:b@h>".to_string(),
        };
        let head = format!(
            "INVITE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: {to}\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
            body_rows(body)
        );
        let mut m =
            msg(at_us, UAC, UAS, Kind::Request { method: "INVITE".to_string() }, 1, to_tag, head);
        m.via_branch = Some(INV_BRANCH.to_string());
        m
    }

    /// An INVITE-transaction response the UAS sends.
    fn inv_resp(at_us: u64, status: u16, extra: &str, body: bool) -> Msg {
        let head = format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 1 INVITE\r\n{extra}{}\r\n",
            body_rows(body)
        );
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some("tb"), head);
        m.via_branch = Some(INV_BRANCH.to_string());
        m
    }

    /// An in-dialog UPDATE the UAS takes (RFC 3311) — a non-INVITE request, so
    /// no provisional it draws may be reliable.
    fn update_on(at_us: u64, extra: &str) -> Msg {
        let head = format!(
            "UPDATE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = msg(
            at_us,
            UAC,
            UAS,
            Kind::Request { method: "UPDATE".to_string() },
            2,
            Some("tb"),
            head,
        );
        m.cseq_method = "UPDATE".to_string();
        m.via_branch = Some(UPDATE_BRANCH.to_string());
        m
    }

    /// The UAS's response on that UPDATE transaction.
    fn update_resp(at_us: u64, status: u16, extra: &str) -> Msg {
        let head = format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
        m.cseq_method = "UPDATE".to_string();
        m.via_branch = Some(UPDATE_BRANCH.to_string());
        m
    }

    /// A PRACK the UAC sends, its `RAck` spelled out.
    fn prack_on(at_us: u64, rack: &str, body: bool) -> Msg {
        let head = format!(
            "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n{}\r\n",
            body_rows(body)
        );
        let mut m = msg(
            at_us,
            UAC,
            UAS,
            Kind::Request { method: "PRACK".to_string() },
            2,
            Some("tb"),
            head,
        );
        m.cseq_method = "PRACK".to_string();
        m.via_branch = Some(PRACK_BRANCH.to_string());
        m
    }

    /// The UAS's response on that PRACK's own server transaction.
    fn prack_answer(at_us: u64, status: u16) -> Msg {
        let head = format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={PRACK_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 PRACK\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
        m.cseq_method = "PRACK".to_string();
        m.via_branch = Some(PRACK_BRANCH.to_string());
        m
    }

    /// The same message crossing the other way — the vantage of a hop that TOOK
    /// what the builders above have it send.
    fn inbound(mut m: Msg) -> Msg {
        std::mem::swap(&mut m.src, &mut m.dst);
        m
    }

    fn decide(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<crate::verdict::Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs, true) })
    }

    /// `require-reliable-1xx-on-require`: the INVITE demanded the extension, so
    /// a plain 18x is neither half of §3's disjunction — and the 420 rejecting
    /// the extension discharges the transaction whenever on it it goes out.
    #[test]
    fn a_plain_18x_against_require_100rel_charges_the_uas() {
        let msgs = vec![
            invite_on(1_000_000, "Require: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, "", false),
        ];
        let f = decide(&RequireReliable1xxOnRequire, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAS);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the provisional");
        let Decision::Violated(Evidence::Unreliable1xx { status, invite_msg, .. }) = &f[0].decision
        else {
            panic!("unreliable-1xx evidence: {:?}", f[0].decision)
        };
        assert_eq!((*status, *invite_msg), (180, 0));

        let reliable = vec![
            invite_on(1_000_000, "Require: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
        ];
        assert!(matches!(
            decide(&RequireReliable1xxOnRequire, &reliable)[0].decision,
            Decision::Compliant
        ));

        let rejected = vec![
            invite_on(1_000_000, "Require: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, "", false),
            inv_resp(1_200_000, 420, "Unsupported: 100rel\r\n", false),
        ];
        assert!(
            decide(&RequireReliable1xxOnRequire, &rejected).iter().all(|f| !f.violated()),
            "the 420 discharges the transaction",
        );
    }

    /// `reliable-needs-client-opt-in`: `Supported` alone licenses the PRACK
    /// machinery; an INVITE this vantage never carried witnesses no
    /// negotiation and settles nothing.
    #[test]
    fn a_reliable_1xx_without_client_opt_in_charges_the_uas() {
        let msgs = vec![
            invite_on(1_000_000, "", None, false),
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
        ];
        let f = decide(&ReliableNeedsClientOptIn, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAS);

        let opted = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
        ];
        assert!(matches!(
            decide(&ReliableNeedsClientOptIn, &opted)[0].decision,
            Decision::Compliant
        ));

        let orphan = vec![inv_resp(1_100_000, 180, &reliable_rows(1), false)];
        assert!(matches!(
            decide(&ReliableNeedsClientOptIn, &orphan)[0].decision,
            Decision::Undecidable(_)
        ));
    }

    /// `no-reliable-1xx-on-in-dialog`: the METHOD of the request it answers is
    /// what makes the provisional an offence (RFC 3262 §3 — "any method but
    /// INVITE"), so an UPDATE is charged and a re-INVITE is not.
    #[test]
    fn a_reliable_1xx_to_a_non_invite_request_charges_the_uas() {
        let msgs = vec![
            update_on(1_000_000, "Supported: 100rel\r\n"),
            update_resp(1_100_000, 183, &reliable_rows(1)),
        ];
        let f = decide(&NoReliable1xxOnInDialog, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::InDialogReliable1xx { method, request_msg, .. }) =
            &f[0].decision
        else {
            panic!("non-INVITE evidence: {:?}", f[0].decision)
        };
        assert_eq!((method.as_str(), *request_msg), ("UPDATE", 0));

        let initial = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
        ];
        assert!(matches!(
            decide(&NoReliable1xxOnInDialog, &initial)[0].decision,
            Decision::Compliant
        ));
    }

    /// A re-INVITE is an INVITE: RFC 3262 §3's To-tag prohibition is written
    /// for a proxy ("unlike a UAS"), and RFC 6141 §4.6 has a UAS answer a
    /// target-refresh re-INVITE with a reliable provisional.
    #[test]
    fn a_reliable_1xx_to_a_re_invite_is_compliant() {
        let msgs = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", Some("tb"), false),
            inv_resp(1_100_000, 183, &reliable_rows(1), false),
        ];
        let f = decide(&NoReliable1xxOnInDialog, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// `unmatched-prack-proxied`: charged to the endpoint that TOOK the PRACK,
    /// and discharged either by an `RSeq` it had taken itself or by the same
    /// `RAck` triple going back out.
    #[test]
    fn a_prack_matching_nothing_local_and_forwarded_nowhere_charges_its_taker() {
        let msgs = vec![
            inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
            prack_on(1_200_000, "9 1 INVITE", false),
        ];
        let f = decide(&UnmatchedPrackProxied, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAS, "the endpoint that took the PRACK is charged");
        let Decision::Violated(Evidence::PrackAbsorbed { rack_rseq, known_rseqs, .. }) =
            &f[0].decision
        else {
            panic!("absorbed evidence: {:?}", f[0].decision)
        };
        assert_eq!((*rack_rseq, known_rseqs.as_slice()), (9, &[1][..]));

        let matched = vec![
            inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
            prack_on(1_200_000, "1 1 INVITE", false),
        ];
        assert!(matches!(
            decide(&UnmatchedPrackProxied, &matched)[0].decision,
            Decision::Compliant
        ));

        let forwarded = vec![
            inbound(inv_resp(1_100_000, 180, &reliable_rows(1), false)),
            prack_on(1_200_000, "9 1 INVITE", false),
            inbound(prack_on(1_250_000, "9 1 INVITE", false)),
        ];
        let out = decide(&UnmatchedPrackProxied, &forwarded);
        assert!(
            out.iter().filter(|f| f.emitter == UAS).all(|f| !f.violated()),
            "forwarded, not absorbed: {out:?}",
        );
    }

    /// `prack-2xx-or-481`: the answer is the UAS's own RSeq state read back,
    /// and `matched` is read as of the PRACK's arrival.
    #[test]
    fn a_prack_draws_2xx_on_a_match_and_481_without_one() {
        let wrong = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "1 1 INVITE", false),
            prack_answer(1_250_000, 481),
        ];
        let f = decide(&Prack2xxOr481, &wrong);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::PrackAnsweredWrongly { status, rack_matched, .. }) =
            &f[0].decision
        else {
            panic!("prack-answer evidence: {:?}", f[0].decision)
        };
        assert_eq!((*status, *rack_matched), (481, true));

        let right = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "1 1 INVITE", false),
            prack_answer(1_250_000, 200),
        ];
        assert!(matches!(decide(&Prack2xxOr481, &right)[0].decision, Decision::Compliant));

        let unmatched = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "9 1 INVITE", false),
            prack_answer(1_250_000, 200),
        ];
        let f = decide(&Prack2xxOr481, &unmatched);
        assert!(f[0].violated(), "no match owes a 481: {:?}", f[0].decision);

        let rejected = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "9 1 INVITE", false),
            prack_answer(1_250_000, 481),
        ];
        assert!(matches!(decide(&Prack2xxOr481, &rejected)[0].decision, Decision::Compliant));
    }

    /// `prack-2xx-or-481`: the match is the whole §7.2 `RAck` — the right
    /// `RSeq` under a CSeq-num of no INVITE this UAS answered reliably, or
    /// under a method no reliable provisional answers, names nothing and owes
    /// the 481.
    #[test]
    fn a_prack_matches_on_every_rack_token_not_the_rseq_alone() {
        let wrong_cseq = |status: u16| {
            vec![
                inv_resp(1_100_000, 180, &reliable_rows(1), false),
                prack_on(1_200_000, "1 101 INVITE", false),
                prack_answer(1_250_000, status),
            ]
        };
        assert!(matches!(
            decide(&Prack2xxOr481, &wrong_cseq(481))[0].decision,
            Decision::Compliant
        ));
        let f = decide(&Prack2xxOr481, &wrong_cseq(200));
        let Decision::Violated(Evidence::PrackAnsweredWrongly {
            status,
            rack_matched,
            rack_rseq,
            ..
        }) = &f[0].decision
        else {
            panic!("prack-answer evidence: {:?}", f[0].decision)
        };
        assert_eq!((*status, *rack_matched, *rack_rseq), (200, false, 1));

        let wrong_method = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            prack_on(1_200_000, "1 1 UPDATE", false),
            prack_answer(1_250_000, 481),
        ];
        assert!(matches!(decide(&Prack2xxOr481, &wrong_method)[0].decision, Decision::Compliant));
    }

    /// `prack-2xx-or-481`: the match is read within the early dialog the PRACK
    /// names. Two forks number their `RSeq`s independently (§3, errata 4600),
    /// so a sibling fork's live number PRACKed in this fork's dialog names
    /// nothing here and owes the 481.
    #[test]
    fn a_sibling_forks_rseq_pracked_in_this_dialog_matches_nothing() {
        let cross_fork = |status: u16| {
            vec![
                fork_resp(1_100_000, 183, &reliable_rows(1), "f1", false),
                fork_resp(1_150_000, 183, &reliable_rows(200), "f2", false),
                fork_prack(1_200_000, "200 1 INVITE", "f1", PRACK_BRANCH),
                prack_answer(1_250_000, status),
            ]
        };
        assert!(matches!(
            decide(&Prack2xxOr481, &cross_fork(481))[0].decision,
            Decision::Compliant
        ));
        let f = decide(&Prack2xxOr481, &cross_fork(200));
        let Decision::Violated(Evidence::PrackAnsweredWrongly { rack_matched, rack_rseq, .. }) =
            &f[0].decision
        else {
            panic!("prack-answer evidence: {:?}", f[0].decision)
        };
        assert_eq!((*rack_matched, *rack_rseq), (false, 200));

        let own_fork = vec![
            fork_resp(1_100_000, 183, &reliable_rows(1), "f1", false),
            fork_resp(1_150_000, 183, &reliable_rows(200), "f2", false),
            fork_prack(1_200_000, "200 1 INVITE", "f2", PRACK_BRANCH),
            prack_answer(1_250_000, 200),
        ];
        assert!(matches!(decide(&Prack2xxOr481, &own_fork)[0].decision, Decision::Compliant));
    }

    /// `delay-2xx-on-unacked-reliable-1xx-with-sdp`: ONE occasion — the 2xx —
    /// naming every RSeq still outstanding; a bodiless provisional put no
    /// description on the wire and is no occasion at all.
    #[test]
    fn the_2xx_waits_for_the_prack_of_an_offer_sent_reliably() {
        let early = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            inv_resp(1_200_000, 200, "", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &early);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 1, "the occasion rests on the 2xx");
        let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
            &f[0].decision
        else {
            panic!("unacked-offer evidence: {:?}", f[0].decision)
        };
        assert_eq!(unacked_rseqs.as_slice(), &[1]);

        let pracked = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            prack_on(1_150_000, "1 1 INVITE", true),
            inv_resp(1_200_000, 200, "", false),
        ];
        assert!(matches!(
            decide(&Delay2xxOnUnackedReliable1xxWithSdp, &pracked)[0].decision,
            Decision::Compliant
        ));

        let bodiless = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            inv_resp(1_200_000, 200, "", false),
        ];
        assert!(decide(&Delay2xxOnUnackedReliable1xxWithSdp, &bodiless).is_empty());

        let two = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            inv_resp(1_150_000, 183, &reliable_rows(2), true),
            inv_resp(1_200_000, 200, "", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &two);
        assert_eq!(f.len(), 1, "one occasion, the 2xx: {f:?}");
        let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
            &f[0].decision
        else {
            panic!("unacked-offer evidence: {:?}", f[0].decision)
        };
        assert_eq!(unacked_rseqs.as_slice(), &[1, 2], "both named on the one finding");
    }

    /// A retransmission of a PRACKed provisional crossing the acknowledgement
    /// on the wire — RFC 3262 §3 has the UAS repeat until the PRACK reaches it
    /// — carries the same `RSeq` and answers to the same PRACK: the 2xx behind
    /// it owes nothing more.
    #[test]
    fn a_retransmission_after_the_prack_reopens_no_obligation() {
        let crossed = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            prack_on(1_150_000, "1 1 INVITE", true),
            inv_resp(1_160_000, 183, &reliable_rows(1), true),
            inv_resp(1_200_000, 200, "", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &crossed);
        assert_eq!(f.len(), 1, "one occasion, the 2xx: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        // A repeat that was never PRACKed at all still leaves the offer open.
        let never = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            inv_resp(1_160_000, 183, &reliable_rows(1), true),
            inv_resp(1_200_000, 200, "", false),
        ];
        let Decision::Violated(Evidence::AnsweredOverUnackedOffer { unacked_rseqs, .. }) =
            &decide(&Delay2xxOnUnackedReliable1xxWithSdp, &never)[0].decision
        else {
            panic!("unacked-offer evidence")
        };
        assert_eq!(unacked_rseqs.as_slice(), &[1], "named once, not once per copy");
    }

    /// **A repeat cannot OPEN an obligation, but it MEETS one** — the wire
    /// model's ruling, and load-bearing here: a vantage that lost the first
    /// copy of a PRACK and kept the retransmission carries that copy as the
    /// whole acknowledgement, so the 2xx behind it owes nothing and the answer
    /// it drew is judged against the transaction it restates.
    #[test]
    fn a_repeat_prack_still_discharges_what_it_acknowledges() {
        let retransmitted = |at_us: u64| {
            let mut p = prack_on(at_us, "1 1 INVITE", false);
            p.repeat = true;
            p
        };
        let answered = vec![
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            retransmitted(1_150_000),
            inv_resp(1_200_000, 200, "", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &answered);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        let judged = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            retransmitted(1_150_000),
            prack_answer(1_200_000, 200),
        ];
        let f = decide(&Prack2xxOr481, &judged);
        assert_eq!(f.len(), 1, "the restated PRACK names the transaction: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A reliable provisional on a chosen early dialog — what a fork looks like
    /// on the branch it shares with its sibling.
    fn fork_resp(at_us: u64, status: u16, extra: &str, to_tag: &str, sdp: bool) -> Msg {
        let head = format!(
            "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={INV_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 1 INVITE\r\n\
             {extra}{}\r\n",
            body_rows(sdp)
        );
        let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 1, Some(to_tag), head);
        m.via_branch = Some(INV_BRANCH.to_string());
        m
    }

    /// The PRACK of one fork's provisional, on that fork's own dialog.
    fn fork_prack(at_us: u64, rack: &str, to_tag: &str, branch: &str) -> Msg {
        let head = format!(
            "PRACK sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={branch}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag={to_tag}\r\nCSeq: 2 PRACK\r\n\
             RAck: {rack}\r\nContent-Length: 0\r\n\r\n"
        );
        let mut m = msg(
            at_us,
            UAC,
            UAS,
            Kind::Request { method: "PRACK".to_string() },
            2,
            Some(to_tag),
            head,
        );
        m.cseq_method = "PRACK".to_string();
        m.via_branch = Some(branch.to_string());
        m
    }

    /// Two forks answer one INVITE on ONE branch and number their `RSeq`s
    /// independently (§12.1.2), so the winning fork's 2xx is judged against its
    /// OWN offer and its own PRACK — never against the abandoned fork's copy of
    /// the same number.
    #[test]
    fn each_fork_answers_for_its_own_offer() {
        let both_pracked = vec![
            fork_resp(1_100_000, 183, &reliable_rows(1), "f1", true),
            fork_prack(1_150_000, "1 1 INVITE", "f1", "z9hG4bK-p1"),
            fork_resp(1_200_000, 183, &reliable_rows(1), "f2", true),
            fork_prack(1_250_000, "1 1 INVITE", "f2", "z9hG4bK-p2"),
            fork_resp(1_300_000, 200, "", "f2", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &both_pracked);
        assert_eq!(f.len(), 1, "one 2xx, one occasion: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        let wrong_fork = vec![
            fork_resp(1_100_000, 183, &reliable_rows(1), "f1", true),
            fork_prack(1_150_000, "1 1 INVITE", "f1", "z9hG4bK-p1"),
            fork_resp(1_200_000, 183, &reliable_rows(1), "f2", true),
            fork_resp(1_300_000, 200, "", "f2", false),
        ];
        let f = decide(&Delay2xxOnUnackedReliable1xxWithSdp, &wrong_fork);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "the winning fork's own offer is unacked: {:?}", f[0].decision);
    }

    /// `prack-accepted-after-final`: the PRACK's own transaction outlives the
    /// INVITE's; one that beat the final is the other rule's occasion.
    #[test]
    fn a_prack_after_the_final_still_draws_2xx() {
        let msgs = vec![
            inv_resp(1_100_000, 200, "", false),
            prack_on(1_200_000, "1 1 INVITE", false),
            prack_answer(1_250_000, 481),
        ];
        let f = decide(&PrackAcceptedAfterFinal, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::LatePrackRejected { status, prior_final_status, .. }) =
            &f[0].decision
        else {
            panic!("late-prack evidence: {:?}", f[0].decision)
        };
        assert_eq!((*status, *prior_final_status), (481, 200));

        let accepted = vec![
            inv_resp(1_100_000, 200, "", false),
            prack_on(1_200_000, "1 1 INVITE", false),
            prack_answer(1_250_000, 200),
        ];
        assert!(matches!(
            decide(&PrackAcceptedAfterFinal, &accepted)[0].decision,
            Decision::Compliant
        ));

        let early = vec![
            prack_on(1_100_000, "1 1 INVITE", false),
            prack_answer(1_150_000, 481),
            inv_resp(1_200_000, 200, "", false),
        ];
        assert!(decide(&PrackAcceptedAfterFinal, &early).is_empty());
    }

    /// `no-new-reliable-1xx-after-final`: a fresh RSeq after the final is the
    /// offence; a retransmission of one the transaction already used is not.
    #[test]
    fn a_new_reliable_1xx_after_the_final_charges_the_uas() {
        let msgs = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            inv_resp(1_200_000, 200, "", false),
            inv_resp(1_300_000, 183, &reliable_rows(2), false),
        ];
        let f = decide(&NoNewReliable1xxAfterFinal, &msgs);
        assert_eq!(f.len(), 2, "both provisionals are occasions: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        let Decision::Violated(Evidence::Reliable1xxAfterFinal {
            rseq, prior_final_status, ..
        }) = &f[1].decision
        else {
            panic!("stray-1xx evidence: {:?}", f[1].decision)
        };
        assert_eq!((*rseq, *prior_final_status), (2, 200));

        let retransmitted = vec![
            inv_resp(1_100_000, 180, &reliable_rows(1), false),
            inv_resp(1_200_000, 200, "", false),
            inv_resp(1_300_000, 180, &reliable_rows(1), false),
        ];
        assert!(decide(&NoNewReliable1xxAfterFinal, &retransmitted).iter().all(|f| !f.violated()));
    }

    /// `no-prack-of-100-trying`: the occasion is the bogus 100 the UAC took,
    /// and a violated one rests on the PRACK it drew.
    #[test]
    fn pracking_a_100_trying_charges_the_uac_at_the_prack() {
        let msgs = vec![
            inv_resp(1_000_000, 100, &reliable_rows(7), false),
            prack_on(1_100_000, "7 1 INVITE", false),
        ];
        let f = decide(&NoPrackOf100Trying, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAC);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the PRACK");
        let Decision::Violated(Evidence::PrackedTrying { rseq, trying_msg, .. }) = &f[0].decision
        else {
            panic!("pracked-trying evidence: {:?}", f[0].decision)
        };
        assert_eq!((*rseq, *trying_msg), (7, 0));

        let ignored = vec![inv_resp(1_000_000, 100, &reliable_rows(7), false)];
        let f = decide(&NoPrackOf100Trying, &ignored);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// `prack-answers-1xx-offer`: the 1xx body is the OFFER only where the
    /// INVITE carried none — the standard setup (offer in the INVITE, answer in
    /// the 183, bodiless PRACK) is no occasion at all.
    #[test]
    fn a_bodiless_prack_answering_a_1xx_offer_charges_its_sender() {
        let msgs = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            prack_on(1_200_000, "1 1 INVITE", false),
        ];
        let f = decide(&PrackAnswers1xxOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, UAC, "the PRACK's sender owes the answer");
        let Decision::Violated(Evidence::PrackWithoutAnswer { rack_rseq, offer_1xx_msg, .. }) =
            &f[0].decision
        else {
            panic!("prack-answer evidence: {:?}", f[0].decision)
        };
        assert_eq!((*rack_rseq, *offer_1xx_msg), (1, 1));

        let answered = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            prack_on(1_200_000, "1 1 INVITE", true),
        ];
        assert!(matches!(
            decide(&PrackAnswers1xxOffer, &answered)[0].decision,
            Decision::Compliant
        ));

        let standard = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, true),
            inv_resp(1_100_000, 183, &reliable_rows(1), true),
            prack_on(1_200_000, "1 1 INVITE", false),
        ];
        assert!(
            decide(&PrackAnswers1xxOffer, &standard).is_empty(),
            "the 183's body is the ANSWER (RFC 3264 §5), so the PRACK owes none",
        );
    }

    /// The extension is invisible to the obligation half: the five rules ported
    /// before it decide the same shape the same way, whether or not the new
    /// server-transaction facts are collected alongside.
    #[test]
    fn the_server_transaction_half_changes_no_obligation_verdict() {
        let msgs = vec![
            invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
            inv_resp(1_100_000, 180, &reliable_rows(5), false),
        ];
        let f = decide(&UnackedReliableProvisional, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "the UAC took it and never PRACKed: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC);
    }
}
