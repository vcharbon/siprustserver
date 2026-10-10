//! The one walk over the wire every round-reading rule of the family reads.

use std::collections::BTreeMap;

use sip_message::sniff;

use crate::wire::{Kind, Msg};

use super::doc::Doc;
use super::records::{
    AckRound, AckSighting, AgentKey, AgentState, AnswerDue, Answered, DelayedAck, OfferStep,
    OriginStep, ReAnswer, Round, RoundKey,
};

/// Whether the description a response carries BINDS as the round's answer on
/// its dialog: a final does (§13.2.1), and so does a reliable provisional — one
/// carrying BOTH `Require: 100rel` and an `RSeq` (RFC 3262 §5, the
/// [`super::prack`](super::super::prack) reading). An unreliable provisional's description is early
/// media the peer may re-latch on and states no answer; a vantage that carried
/// no header bytes cannot tell, and the conservative reading is that it binds
/// nothing.
fn binds(status: u16, msg: &Msg) -> bool {
    if status >= 200 {
        return true;
    }
    msg.head.as_deref().is_some_and(|h| sniff::require_has_100rel(h) && sniff::rseq_of(h).is_some())
}

/// Whether a method's description is an OFFER whose answer rides the 2xx to
/// that same transaction: `INVITE` (§13.2.1) and `UPDATE` (RFC 3311 §5.1). A
/// description on any other method states no round the response owes an answer
/// to — `OPTIONS` carries a capability set (§11.2), and a `PRACK` body is the
/// ANSWER to the reliable provisional's offer as often as an offer of its own
/// (RFC 3262 §5), which this reading cannot tell apart.
fn offer_answer_method(method: &str) -> bool {
    ["INVITE", "UPDATE"].iter().any(|m| method.eq_ignore_ascii_case(m))
}

/// What one view says about the offer/answer exchanges it carried.
///
/// The occasion collections are the obligations' subjects; `docs` is the
/// per-message description all of them read facts off — `m=` list, `t=` line,
/// direction attributes, rtpmaps and ports included — parsed once. A rule that
/// needs a fact about a message reaches it by that message's view index.
///
/// Two of the collections are TIMELINES rather than end states, because their
/// readers judge what an agent knew when it acted: `offer_steps` says which of
/// an agent's own rounds were still open at the moment it sent each offer (an
/// agent's `answered` map is its final state and cannot answer that), and
/// `sent_streams` is the agent's own descriptions in send order whatever their
/// `o=` lines say (`origin_steps` skips the ones no origin reader accepts).
#[derive(Default)]
pub(super) struct Reading<'a> {
    pub(super) docs: Vec<Option<Doc>>,
    pub(super) acks: Vec<AckSighting<'a>>,
    pub(super) answers: Vec<Answered<'a>>,
    pub(super) origin_steps: Vec<OriginStep<'a>>,
    /// Descriptions an agent sent whose `o=` line no reader accepts, after its
    /// origin stream had already started.
    pub(super) unreadable_origins: Vec<OriginStep<'a>>,
    /// Every offer an agent SENT, with what was pending when it went out. Read
    /// by [`NoNewOfferWhileOfferPending`](super::NoNewOfferWhileOfferPending).
    pub(super) offer_steps: Vec<OfferStep<'a>>,
    /// Every non-failure final an agent sent on a round it took an offer for,
    /// in send order. Read by [`Final2xxAnswersTheOffer`](super::Final2xxAnswersTheOffer).
    pub(super) answers_due: Vec<AnswerDue<'a>>,
    /// Every binding description an agent sent onto a dialog whose answer it
    /// had already stated, in send order. Read by
    /// [`SecondAnswerRepeatsTheFirst`](super::SecondAnswerRepeatsTheFirst).
    pub(super) re_answers: Vec<ReAnswer<'a>>,
    /// Every ACK an agent sent acknowledging a delayed-offer 2xx — the round's
    /// answer is owed in it — with the offer it owed. Read by
    /// [`DelayedOfferAnsweredInAck`](super::DelayedOfferAnsweredInAck).
    pub(super) delayed_acks: Vec<DelayedAck<'a>>,
    /// Per agent and call, the indexes into the view's `msgs` of the
    /// descriptions that agent SENT, ascending. Read by
    /// [`ReOfferMLineCountMonotonic`](super::ReOfferMLineCountMonotonic) (consecutive pairs) and
    /// [`PayloadTypeMappingStable`](super::PayloadTypeMappingStable) (the payload-type bindings the whole stream
    /// states).
    pub(super) sent_streams: Vec<Vec<usize>>,
}

impl<'a> Reading<'a> {
    pub(super) fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading { docs: msgs.iter().map(Doc::of).collect(), ..Reading::default() };
        let mut agents: BTreeMap<AgentKey<'a>, AgentState<'a>> = BTreeMap::new();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(&mut agents, mi, msg, msg.src.as_str(), true);
            if msg.dst != msg.src {
                seen.absorb(&mut agents, mi, msg, msg.dst.as_str(), false);
            }
        }
        seen.sent_streams = agents.into_values().map(|s| s.sent_docs).collect();
        seen
    }

    /// The agent's own most recent offer still awaiting an answer, as of the
    /// point the walk has reached — `None` where none is outstanding.
    fn pending_offer<'s>(state: &'s AgentState<'a>) -> Option<&'s Round<'a>> {
        state.rounds.iter().rev().find(|r| r.offer_sent && r.answered.is_empty())
    }

    /// The offer and the answer of one round an agent closed. Both are present
    /// by construction — a round opens and closes on a message the vantage
    /// carried a readable description for — so an answer is never an occasion
    /// whose facts the vantage withheld.
    pub(super) fn round_docs(&self, a: &Answered<'_>) -> Option<(&Doc, &Doc)> {
        Some((self.docs[a.offer].as_ref()?, self.docs[a.msg].as_ref()?))
    }

    /// Record an offer about to open a round, with the agent's own round that
    /// was still open at that moment. Called BEFORE the new round is pushed, so
    /// the pending one is a genuine predecessor; a taken offer states no act of
    /// this agent's and is passed over.
    fn note_offer(&mut self, state: &AgentState<'a>, mi: usize, msg: &'a Msg, sent: bool) {
        if !sent {
            return;
        }
        let pending = Reading::pending_offer(state);
        self.offer_steps.push(OfferStep {
            msg: mi,
            src: msg.src.as_str(),
            dst: msg.dst.as_str(),
            cseq: msg.cseq,
            pending: pending.map(|r| r.offer),
            pending_cseq: pending.map_or(0, |r| r.offer_cseq),
        });
    }

    /// Absorb one message into ONE agent's state. `sent` says which side of the
    /// hop that agent is on: the same bytes open a round the sender drove and a
    /// round its peer took.
    ///
    /// **A repeat states no new act.** Retransmitting until answered is
    /// required behaviour (§17), so a repeated offer re-opens nothing, a
    /// repeated answer closes nothing twice, and a repeated ACK is not a second
    /// ACK — the whole absorb is skipped for one.
    fn absorb(
        &mut self,
        agents: &mut BTreeMap<AgentKey<'a>, AgentState<'a>>,
        mi: usize,
        msg: &'a Msg,
        agent: &'a str,
        sent: bool,
    ) {
        if msg.repeat {
            return;
        }
        let key = AgentKey { agent, call_id: msg.call_id.as_str() };
        let state = agents.entry(key).or_default();
        let dialog = msg.to_tag.as_deref().unwrap_or_default();
        let has_doc = self.docs[mi].is_some();

        if sent && has_doc {
            state.sent_docs.push(mi);
            let readable = self.docs[mi].as_ref().is_some_and(|d| d.origin.is_some());
            if let Some(prior) = state.last_origin {
                let step = OriginStep {
                    msg: mi,
                    src: msg.src.as_str(),
                    dst: msg.dst.as_str(),
                    cseq: msg.cseq,
                    prior,
                };
                if readable {
                    self.origin_steps.push(step);
                } else {
                    self.unreadable_origins.push(step);
                }
            }
            if readable {
                state.last_origin = Some(mi);
            }
        }

        match &msg.kind {
            Kind::Request { method } if method.eq_ignore_ascii_case("ACK") => {
                // An ACK rides the INVITE transaction, and the agent that sent
                // the ACK is the one that sent that INVITE.
                let round_key = RoundKey {
                    cseq: msg.cseq,
                    method: "INVITE".to_string(),
                    request_sent_by_agent: sent,
                };
                let round = state.round_at.get(&round_key).map(|i| &state.rounds[*i]);
                let slot = (msg.cseq, sent, dialog);
                if sent && !state.offered_reliably.contains(&slot) {
                    if let Some(ri) = state.delayed_at.get(&slot) {
                        self.delayed_acks.push(DelayedAck {
                            msg: mi,
                            src: msg.src.as_str(),
                            dst: msg.dst.as_str(),
                            cseq: msg.cseq,
                            offer: state.rounds[*ri].offer,
                            answered: has_doc,
                            body_known: msg.body.is_some(),
                        });
                    }
                }
                let outcome = match (round, msg.body.is_some(), has_doc) {
                    (_, false, _) => AckRound::BodyUnknown,
                    (_, true, false) => AckRound::NoDescription,
                    (Some(r), true, true) => match r.answered.get(dialog) {
                        Some(answer) => AckRound::Closed { offer: r.offer, answer: *answer },
                        None => AckRound::Open,
                    },
                    (None, true, true) => {
                        // §13.2.1 delayed offer: the 2xx carried it and this ACK
                        // carries the answer, which is the round's second half,
                        // not a third description.
                        match state.delayed_at.remove(&(msg.cseq, sent, dialog)) {
                            Some(ri) => {
                                state.rounds[ri].answered.insert(dialog, mi);
                                if sent {
                                    let offer = state.rounds[ri].offer;
                                    let offer_cseq = state.rounds[ri].offer_cseq;
                                    self.answers.push(Answered {
                                        msg: mi,
                                        src: msg.src.as_str(),
                                        dst: msg.dst.as_str(),
                                        offer,
                                        offer_cseq,
                                    });
                                }
                                AckRound::Open
                            }
                            None => AckRound::RoundUnknown,
                        }
                    }
                };
                if !sent {
                    return; // the ACK's sender owns the occasion, on its own slot
                }
                self.acks.push(AckSighting {
                    msg: mi,
                    src: msg.src.as_str(),
                    dst: msg.dst.as_str(),
                    cseq: msg.cseq,
                    round: outcome,
                });
            }
            Kind::Request { method } => {
                if !has_doc {
                    return;
                }
                let round_key = RoundKey {
                    cseq: msg.cseq,
                    method: method.to_ascii_uppercase(),
                    request_sent_by_agent: sent,
                };
                if state.round_at.contains_key(&round_key) {
                    return;
                }
                self.note_offer(state, mi, msg, sent);
                state.round_at.insert(round_key, state.rounds.len());
                state.rounds.push(Round {
                    offer: mi,
                    offer_cseq: msg.cseq,
                    offer_sent: sent,
                    offer_dialog: dialog,
                    answered: BTreeMap::new(),
                    bound: BTreeMap::new(),
                });
            }
            Kind::Response { status } => {
                if !(101..300).contains(status) {
                    // A 100 opens nothing and a failure final closes nothing.
                    return;
                }
                // A response travels opposite to the request it answers.
                let request_sent_by_agent = !sent;
                let round_key = RoundKey {
                    cseq: msg.cseq,
                    method: msg.cseq_method.to_ascii_uppercase(),
                    request_sent_by_agent,
                };
                match state.round_at.get(&round_key).copied() {
                    Some(ri) => {
                        let same_dialog = state.rounds[ri].offer_dialog.is_empty()
                            || state.rounds[ri].offer_dialog == dialog;
                        if sent
                            && same_dialog
                            && *status >= 200
                            && offer_answer_method(&msg.cseq_method)
                        {
                            self.answers_due.push(AnswerDue {
                                msg: mi,
                                src: msg.src.as_str(),
                                dst: msg.dst.as_str(),
                                status: *status,
                                offer: state.rounds[ri].offer,
                                offer_cseq: state.rounds[ri].offer_cseq,
                                bound: state.rounds[ri].bound.get(dialog).copied(),
                            });
                        }
                        if !has_doc {
                            return;
                        }
                        if binds(*status, msg) {
                            match state.rounds[ri].bound.get(dialog).copied() {
                                // The dialog's answer is stated; a second
                                // binding description on it is its own occasion.
                                Some(first) if sent => {
                                    let offer = state.rounds[ri].offer;
                                    let offer_cseq = state.rounds[ri].offer_cseq;
                                    self.re_answers.push(ReAnswer {
                                        msg: mi,
                                        src: msg.src.as_str(),
                                        dst: msg.dst.as_str(),
                                        offer,
                                        offer_cseq,
                                        first,
                                    });
                                }
                                Some(_) => {}
                                None => {
                                    state.rounds[ri].bound.insert(dialog, mi);
                                }
                            }
                        }
                        if state.rounds[ri].answered.contains_key(dialog) {
                            return;
                        }
                        state.rounds[ri].answered.insert(dialog, mi);
                        if !sent {
                            return; // the answerer's occasion, on the answerer's slot
                        }
                        let offer = state.rounds[ri].offer;
                        let offer_cseq = state.rounds[ri].offer_cseq;
                        self.answers.push(Answered {
                            msg: mi,
                            src: msg.src.as_str(),
                            dst: msg.dst.as_str(),
                            offer,
                            offer_cseq,
                        });
                    }
                    None => {
                        // No offer on this transaction: a 2xx to an INVITE is
                        // the delayed offer, answered in the ACK.
                        if !has_doc || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                            return;
                        }
                        let slot = (msg.cseq, request_sent_by_agent, dialog);
                        if *status < 200 {
                            if binds(*status, msg) {
                                state.offered_reliably.insert(slot);
                            }
                            return;
                        }
                        if state.delayed_at.contains_key(&slot) {
                            return;
                        }
                        self.note_offer(state, mi, msg, sent);
                        state.delayed_at.insert(slot, state.rounds.len());
                        state.rounds.push(Round {
                            offer: mi,
                            offer_cseq: msg.cseq,
                            offer_sent: sent,
                            offer_dialog: dialog,
                            answered: BTreeMap::new(),
                            bound: BTreeMap::new(),
                        });
                    }
                }
            }
        }
    }
}
