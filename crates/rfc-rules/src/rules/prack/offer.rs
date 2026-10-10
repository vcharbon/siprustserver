//! A session description carried by a reliable provisional (RFC 3262 §3, §5).

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;

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
