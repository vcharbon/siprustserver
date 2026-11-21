//! A session description carried by a reliable provisional (RFC 3262 §3, §5).

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::reading::Reading;

/// **RFC 3262 §3 — the 2xx waits for the PRACK of a reliable provisional
/// that carried a session description.** That description, offer or answer,
/// is on the wire for the PRACK to confirm; answering the INVITE before that
/// confirmation arrives establishes a dialog over a description neither side
/// has agreed.
///
/// The occasion is ONE INVITE 2xx the UAS sent on a transaction where it had
/// already sent a reliable provisional carrying a description
/// ([`Msg::carries_description`](crate::wire::Msg::carries_description); a body framing none, or of another type,
/// carries none) — one finding naming EVERY `RSeq` still outstanding, since
/// the 2xx is the single act judged. A provisional whose body the vantage did
/// not record leaves the occasion `Undecidable` where no recorded one opens
/// it. Each `RSeq` is judged once, from its first emission: §3 has the UAS
/// repeat the provisional until the PRACK reaches it, so a retransmission
/// crossing the acknowledgement on the wire carries the same `RSeq`, answers
/// to the same PRACK, and re-opens nothing. The PRACK names the `RSeq` and
/// the INVITE CSeq on the early dialog, and is either taken at the UAS's
/// address (it may come straight from a UAC behind a proxy) or sent by the
/// provisional's taker (it reaches the UAS at its Contact, §4, which need not
/// be the address it answers from). Charges the UAS.
pub struct Delay2xxOnUnackedReliable1xxWithSdp;

impl Obligation for Delay2xxOnUnackedReliable1xxWithSdp {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, list) in &seen.answers {
            for a in list.iter().filter(|a| a.answers("INVITE") && (200..300).contains(&a.status)) {
                let mut offer_1xx_msg = None;
                let mut unrecorded = false;
                let mut unacked: Vec<u64> = Vec::new();
                let mut judged: BTreeSet<u64> = BTreeSet::new();
                // The 2xx confirms ONE early dialog; an abandoned fork's offer
                // is not what it answers, and each fork numbers its own RSeqs.
                for q in list
                    .iter()
                    .filter(|q| q.reliable_1xx() && q.to_tag == a.to_tag && q.ts_us < a.ts_us)
                {
                    match q.description {
                        Some(true) => {}
                        Some(false) => continue,
                        None => {
                            unrecorded = true;
                            continue;
                        }
                    }
                    offer_1xx_msg.get_or_insert(q.msg);
                    let Some(rseq) = q.rseq else { continue };
                    if !judged.insert(rseq) {
                        continue;
                    }
                    let acked = seen.pracked_between(
                        (key.uas, q.taker),
                        (key.call_id, a.to_tag, q.cseq, rseq),
                        q.ts_us,
                        a.ts_us,
                    );
                    if !acked {
                        unacked.push(rseq);
                    }
                }
                let finding = |decision| Finding {
                    rule: RuleId::Delay2xxOnUnackedReliable1xxWithSdp,
                    emitter: key.uas.to_string(),
                    taker: a.taker.to_string(),
                    cseq: a.cseq,
                    relayed: false,
                    anchor: a.msg,
                    decision,
                };
                let Some(offer_1xx_msg) = offer_1xx_msg else {
                    if unrecorded {
                        out.push(finding(Decision::Undecidable(
                            "a reliable provisional's body is not at this vantage",
                        )));
                    }
                    continue;
                };
                out.push(finding(if unacked.is_empty() {
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
                }));
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
/// **A description in a reliable provisional is the OFFER only where its
/// INVITE carried none** (RFC 3264 §5). With an offer already in the INVITE
/// the provisional's description is the ANSWER and the PRACK owes none — the
/// standard PRACK call setup. Each side reads a session description
/// ([`Msg::carries_description`](crate::wire::Msg::carries_description)),
/// never a body: a body framing none, or of another type, is neither offer
/// nor answer. An INVITE this vantage never carried keeps the conservative
/// reading (a provisional's description is an offer); an INVITE, provisional
/// or PRACK whose body the vantage did not record leaves the occasion
/// `Undecidable`.
///
/// The occasion is ONE PRACK, keyed by the offer's `RSeq` in its early dialog
/// (the To tag, which both directions carry) rather than by direction: the
/// offer and the answer are two ends of one negotiation and either party's
/// copy is the same fact, and each fork numbers its `RSeq`s on its own (§3,
/// errata 4600). Charges the PRACK's sender.
///
/// **This reads DESCRIPTION PRESENCE, never the offer/answer state machine**
/// — it walks the view directly rather than through the family's `Reading`,
/// because the offer is a fact about the CALL and not about either endpoint's
/// own server transactions.
pub struct PrackAnswers1xxOffer;

/// A reliable provisional's description read as an offer: where it was first
/// seen, and whether this vantage recorded enough to decide its PRACK.
struct Offer {
    msg: usize,
    decidable: bool,
}

impl Obligation for PrackAnswers1xxOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per INVITE (call and CSeq): whether it carried the offer, `None`
        // where a copy's body is not at this vantage; and each offer-bearing
        // reliable provisional's `RSeq`.
        let mut invite_offered: BTreeMap<(&str, u32), Option<bool>> = BTreeMap::new();
        let mut offers: BTreeMap<(&str, &str, u64), Offer> = BTreeMap::new();
        let mut out = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let head = msg.head.as_deref();
            let call = msg.call_id.as_str();
            if msg.is_request("INVITE") {
                let offered = invite_offered.entry((call, msg.cseq)).or_insert(Some(false));
                *offered = match (*offered, msg.carries_description()) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (None, _) | (_, None) => None,
                    (Some(false), Some(false)) => Some(false),
                };
                continue;
            }
            if msg.is_request("PRACK") {
                let Some(rseq) = head.and_then(sniff::rack_rseq) else { continue };
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                let Some(offer) = offers.get(&(call, dialog, rseq)) else { continue };
                let answered = msg.carries_description().filter(|_| offer.decidable);
                out.push(Finding {
                    rule: RuleId::PrackAnswers1xxOffer,
                    emitter: msg.src.to_string(),
                    taker: msg.dst.to_string(),
                    cseq: msg.cseq,
                    relayed: false,
                    anchor: mi,
                    decision: match answered {
                        Some(true) => Decision::Compliant,
                        Some(false) => Decision::Violated(Evidence::PrackWithoutAnswer {
                            bodiless_prack_msg: mi,
                            bodiless_prack_hop: msg.hop,
                            bodiless_prack_ts_us: msg.at_us,
                            rack_rseq: rseq,
                            offer_1xx_msg: offer.msg,
                        }),
                        None => {
                            Decision::Undecidable("an offer or answer body is not at this vantage")
                        }
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
            let described = msg.carries_description();
            if !sniff::require_has_100rel(head) || described == Some(false) {
                continue;
            }
            let invite = invite_offered.get(&(call, msg.cseq)).copied();
            if invite == Some(Some(true)) {
                continue; // the INVITE carried the offer: this description is the answer
            }
            if let Some(rseq) = sniff::rseq_of(head) {
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                let decidable = described.is_some() && invite != Some(None);
                offers.entry((call, dialog, rseq)).or_insert(Offer { msg: mi, decidable });
            }
        }
        out
    }
}
