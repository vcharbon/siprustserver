//! The RFC 3261 §13.2 round: an offer is answered once, in the right message,
//! and no new offer overtakes a pending one.

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use crate::rules::Obligation;

use super::doc::{media_plan, Doc};
use super::reading::Reading;
use super::records::AckRound;

/// **RFC 3261 §13.2.2.4 — an ACK closing a completed offer/answer exchange
/// carries NO body.** When the INVITE carried the offer and a non-failure
/// response carried the answer, the round is over: §13.2.1 forbids a further
/// offer on that INVITE transaction, and the ACK has no answer left to deliver.
/// A body there is either a third session description nobody asked for or a
/// default body leaking out of the sender — both leave the two ends disagreeing
/// on the negotiated media.
///
/// Charges the ACK's SENDER. Every ACK it sent is an occasion: one carrying no
/// description meets the obligation, one on a still-open round carries the
/// answer the round is waiting for, and one whose round this vantage never
/// carried settles nothing.
pub struct AckBodyAfterCompleteOfferAnswer;

impl Obligation for AckBodyAfterCompleteOfferAnswer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for ack in &seen.acks {
            let head = |decision| Finding {
                rule: RuleId::AckBodyAfterCompleteOfferAnswer,
                emitter: ack.src.to_string(),
                taker: ack.dst.to_string(),
                cseq: ack.cseq,
                relayed: false,
                anchor: ack.msg,
                decision,
            };
            out.push(head(match ack.round {
                AckRound::BodyUnknown => {
                    Decision::Undecidable("the vantage carried no body bytes for this ACK")
                }
                AckRound::RoundUnknown => Decision::Undecidable(
                    "the vantage did not carry the offer/answer round this ACK closes",
                ),
                AckRound::NoDescription | AckRound::Open => Decision::Compliant,
                AckRound::Closed { offer, answer } => {
                    Decision::Violated(Evidence::AckBodyOnClosedRound {
                        ack_body_msg: ack.msg,
                        ack_body_hop: wire.msgs[ack.msg].hop,
                        ack_body_ts_us: wire.msgs[ack.msg].at_us,
                        offer_msg: offer,
                        answer_msg: answer,
                        streams: seen.docs[ack.msg]
                            .as_ref()
                            .map(Doc::stream_table)
                            .unwrap_or_default(),
                    })
                }
            }));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §13.2.1 / RFC 3264 §6 — an offer is ANSWERED.** The answer to an
/// offer a request carried rides a reliable non-failure message correlated to
/// that request, and the 2xx is the last one there is: a 2xx carrying no
/// description over an offer no reliable message has answered ends the round
/// with the two ends holding different media plans, and the peer has nothing
/// left to latch on.
///
/// The occasion is a 2xx an agent sent on a transaction whose request it took
/// an offer on — `INVITE` or `UPDATE` (`offer_answer_method`) — and on the
/// dialog that offer named, since two early dialogs number their requests
/// independently (§12.2.1.1). It is met by a description on that 2xx, or by one
/// that already BOUND the dialog (`binds`): a reliable provisional's answer
/// discharges the 2xx, while early media in an unreliable provisional is a plan
/// the peer may re-latch on (RFC 3960) and binds nothing.
///
/// A failure final owes no answer and is no occasion, nor is a round the
/// vantage never carried the offer of, nor one whose transaction the recording
/// never sees finished — the charge rests on a 2xx that arrived, never on a
/// silence. A delayed offer (§13.2.1) is the 2xx itself and is answered in the
/// ACK, a different round with a different answerer, so it opens no occasion
/// here.
///
/// Charges the ANSWERER, on the offer's transaction, anchored at the 2xx it
/// sent.
pub struct Final2xxAnswersTheOffer;

impl Obligation for Final2xxAnswersTheOffer {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for due in &seen.answers_due {
            let decision = if due.bound.is_some() || seen.docs[due.msg].is_some() {
                Decision::Compliant
            } else {
                match wire.msgs[due.msg].body.as_deref() {
                    None => {
                        Decision::Undecidable("the vantage carried no body bytes for this final")
                    }
                    Some([]) => Decision::Violated(Evidence::OfferLeftUnanswered {
                        unanswered_final_msg: due.msg,
                        unanswered_final_hop: wire.msgs[due.msg].hop,
                        unanswered_final_ts_us: wire.msgs[due.msg].at_us,
                        status: due.status,
                        offer_msg: due.offer,
                        offered_streams: seen.docs[due.offer]
                            .as_ref()
                            .map(Doc::stream_table)
                            .unwrap_or_default(),
                    }),
                    Some(_) => Decision::Undecidable(
                        "the final carried bytes this reading does not read as a description",
                    ),
                }
            };
            out.push(Finding {
                rule: RuleId::Final2xxAnswersTheOffer,
                emitter: due.src.to_string(),
                taker: due.dst.to_string(),
                cseq: due.offer_cseq,
                relayed: false,
                anchor: due.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §13.2.2.4 / §13.2.1 — a delayed offer is answered in the ACK.**
/// Where the INVITE carried no offer, the 2xx carries it, and the ACK MUST
/// carry the answer: it is the last message of the round, so an ACK without
/// one leaves the two ends with no agreed session at all.
///
/// The occasion is an ACK an agent sent on a delayed-offer round it was
/// presented (`delayed_at`): met by a description on the ACK. An ACK whose body
/// bytes the vantage never carried is undecided. Charges the ACK's sender,
/// anchored at the ACK.
pub struct DelayedOfferAnsweredInAck;

impl Obligation for DelayedOfferAnsweredInAck {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out: Vec<Finding> = seen
            .delayed_acks
            .iter()
            .map(|ack| {
                let decision = if ack.answered {
                    Decision::Compliant
                } else if !ack.body_known {
                    Decision::Undecidable("the vantage carried no body bytes for this ACK")
                } else {
                    Decision::Violated(Evidence::DelayedOfferUnanswered {
                        unanswering_ack_msg: ack.msg,
                        unanswering_ack_hop: wire.msgs[ack.msg].hop,
                        unanswering_ack_ts_us: wire.msgs[ack.msg].at_us,
                        delayed_offer_msg: ack.offer,
                    })
                };
                Finding {
                    rule: RuleId::DelayedOfferAnsweredInAck,
                    emitter: ack.src.to_string(),
                    taker: ack.dst.to_string(),
                    cseq: ack.cseq,
                    relayed: false,
                    anchor: ack.msg,
                    decision,
                }
            })
            .collect();
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §13.2.1 — one dialog carries ONE answer.** The peer treats the
/// answer it receives as the answer and ignores every later description on that
/// dialog, so a second one there either re-states the same plan (which §13.2.1
/// admits) or states a plan only its sender is acting on, leaving the two ends
/// sending to different places.
///
/// The occasion is a description that BINDS — a final, or a reliable
/// provisional (`binds`) — on a dialog where one already did. Early media in
/// an unreliable provisional binds nothing: an announcement source the peer may
/// re-latch on before the callee answers is RFC 3960's shape, not this
/// obligation's.
///
/// Two forks of one INVITE are two dialogs and two answers to one offer: this
/// obligation is per dialog and never charges that. A B2BUA that COLLAPSES a
/// fork onto one To tag is what it charges.
///
/// Charges the second description's SENDER, on the offer's transaction, over
/// the transport plan rather than byte for byte — a plan re-stated under
/// another origin is [`SdpOriginContinuity`](super::SdpOriginContinuity)'s occasion, and a second
/// description an ACK carries is [`AckBodyAfterCompleteOfferAnswer`]'s.
pub struct SecondAnswerRepeatsTheFirst;

impl Obligation for SecondAnswerRepeatsTheFirst {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for r in &seen.re_answers {
            let (Some(first), Some(second)) =
                (seen.docs[r.first].as_ref(), seen.docs[r.msg].as_ref())
            else {
                continue;
            };
            let (first_plan, second_plan) = (media_plan(first), media_plan(second));
            let decision = if first_plan == second_plan {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::SecondAnswerDiverged {
                    second_answer_msg: r.msg,
                    second_answer_hop: wire.msgs[r.msg].hop,
                    second_answer_ts_us: wire.msgs[r.msg].at_us,
                    offer_msg: r.offer,
                    first_answer_msg: r.first,
                    first_plan,
                    second_plan,
                })
            };
            out.push(Finding {
                rule: RuleId::SecondAnswerRepeatsTheFirst,
                emitter: r.src.to_string(),
                taker: r.dst.to_string(),
                cseq: r.offer_cseq,
                relayed: false,
                anchor: r.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §5 — an agent holds ONE offer of its own outstanding at a time on
/// a call.** An offer is answered before the next one goes out: the two ends
/// agree on the media by walking rounds one at a time, and a second offer over
/// an unanswered one (glare) leaves them applying different descriptions to the
/// same session.
///
/// Charges the offer's SENDER, on every offer it sends. What discharges the
/// prior offer is its answer — the first non-failure response on that
/// transaction carrying a description, or (delayed offer, §13.2.1) the ACK.
/// A round the agent merely TOOK is the peer's to answer and never blocks: the
/// peer's own overtaking offer is the peer's occasion, on the peer's slot.
/// An offer this vantage carried no body for opens no round and is no occasion.
pub struct NoNewOfferWhileOfferPending;

impl Obligation for NoNewOfferWhileOfferPending {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for s in &seen.offer_steps {
            let decision = match s.pending {
                None => Decision::Compliant,
                Some(pending) => Decision::Violated(Evidence::OfferWhilePending {
                    new_offer_msg: s.msg,
                    new_offer_hop: wire.msgs[s.msg].hop,
                    new_offer_ts_us: wire.msgs[s.msg].at_us,
                    pending_offer_msg: pending,
                    pending_offer_cseq: s.pending_cseq,
                }),
            };
            out.push(Finding {
                rule: RuleId::NoNewOfferWhileOfferPending,
                emitter: s.src.to_string(),
                taker: s.dst.to_string(),
                cseq: s.cseq,
                relayed: false,
                anchor: s.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
