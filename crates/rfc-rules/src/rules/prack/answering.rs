//! The PRACK's own transaction (RFC 3262 §3): who answers it, and with what.

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;
use super::records::{Served, TxnKey};

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
/// ARRIVAL and still held unacknowledged — an `RSeq` it sends afterwards was
/// not yet its to acknowledge, one an earlier PRACK of another transaction
/// already drew a 2xx for is off the unacknowledged list, the right `RSeq`
/// under another INVITE's CSeq names nothing, and a sibling fork's number
/// names nothing here (§3, errata 4600). Charges the UAS.
pub struct Prack2xxOr481;

impl Obligation for Prack2xxOr481 {
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
                    .contains(&(rack.rseq, rack.cseq))
                && !acknowledged_before(&seen, key, prack);
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

/// Whether, before `prack` arrived, the UAS of `key` had answered 2xx to a
/// PRACK of another transaction naming the same `RAck` in the same dialog: the
/// provisional was acknowledged then, and is on no unacknowledged list now.
fn acknowledged_before(seen: &Reading<'_>, key: &TxnKey<'_>, prack: &Served<'_>) -> bool {
    seen.served.iter().any(|(other, earlier)| {
        other.uas == key.uas
            && other.call_id == key.call_id
            && other.branch != key.branch
            && earlier.method.eq_ignore_ascii_case("PRACK")
            && earlier.dialog == prack.dialog
            && earlier.rack == prack.rack
            && earlier.ts_us < prack.ts_us
            && seen.answers.get(other).is_some_and(|list| {
                list.iter().any(|a| {
                    a.answers("PRACK") && (200..300).contains(&a.status) && a.ts_us < prack.ts_us
                })
            })
    })
}
