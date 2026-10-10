//! The UAS's `RSeq` space (RFC 3262 §3): one reliable provisional outstanding
//! at a time, numbered contiguously, none new after the final.

use std::collections::BTreeSet;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;
use super::records::{Answer, Reliable};

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
