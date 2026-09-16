//! The offer/answer family: the RFC 3261 §13.2 / RFC 3264 obligations that
//! need to know WHICH ROUND a session description belongs to, and the one
//! reading of the wire they share.
//!
//! **The reading is a per-AGENT state machine, and every message feeds two of
//! them.** A description is one endpoint's emission and the other's arrival, so
//! the walk absorbs each message twice — once as `src`, once as `dst` — and
//! each agent keeps its own rounds. A round opens when a request carries a
//! description and closes when the first non-failure response to that
//! transaction carries one; a request WITHOUT a description whose 2xx carries
//! one is the delayed-offer form (§13.2.1), where the 2xx is the offer and the
//! ACK is the answer.
//!
//! **The round is keyed by the transaction AND the direction; the ANSWER is
//! keyed by the early dialog.** Both ends of a dialog number their requests
//! independently (§12.2), so which end sent the offer is part of the round's
//! identity; two forks answering one INVITE are two answers to one offer, so
//! the To tag partitions the closures (the [`super::prack`] precedent).
//! A second description INSIDE one partition closes nothing: the round keeps
//! the answer the peer acted on, and a second BINDING one is its own occasion —
//! [`SecondAnswerRepeatsTheFirst`]'s.
//!
//! **Every occasion here judges what an agent SENT**, so a finding always names
//! the party at fault and one wire message is never judged twice — once from
//! each end. What a message the agent merely TOOK does is the peer's occasion,
//! on the peer's own slot.
//!
//! **A hop's exemption is the CONSUMER's.** Whether a lane merely relays a
//! stream is a property of that lane across the whole recording, not of the
//! message in hand: a transparent proxy carries both agents' descriptions on
//! one Call-ID and has no offer/answer state of its own. These rules leave
//! `relayed` false and the consumer skips the lanes it knows to be relays — the
//! [`super::final_response`] precedent.
//!
//! **A body-less vantage decides nothing.** [`Msg::body`] is `None` where the
//! vantage did not carry the bytes; a rule that needs a body fact there returns
//! `Undecidable`, never a guess. `Some(&[])` is the opposite — the message
//! demonstrably carried no body.
//!
//! **Two of the obligations here need no round at all** —
//! [`SdpBodyParseable`] and [`C0PortNonZero`] judge ONE description on its own
//! bytes — and they still live in this file: RFC 3264 is one family, every
//! `sdp_doc` reading in this crate is here, and they charge the sender exactly
//! as every rule above does.
//!
//! SDP grammar is [`sip_message::sdp_doc`]'s and [`sip_message::sdp`]'s: this
//! module reads a description, it never parses one.

use std::collections::BTreeMap;

use sip_message::sdp_doc::{self, SdpDirection, SdpDoc, SdpOrigin};
use sip_message::{sdp, sniff};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

// ---------------------------------------------------------------------------
// The shared reading
// ---------------------------------------------------------------------------

/// One session description as the view carried it.
struct Doc {
    doc: SdpDoc,
    /// Its `o=` line, absent where the description spells none §5.2 accepts.
    origin: Option<SdpOrigin>,
}

impl Doc {
    /// The description a message carried, or `None` where the vantage carried
    /// no body bytes or the message declared no session description. A body
    /// with no `Content-Type` declares none (RFC 3261 §20.15): `wellformed`
    /// charges that once, and this family reads the round as it was declared.
    fn of(msg: &Msg) -> Option<Doc> {
        let body = msg.sdp()?;
        let doc = sdp_doc::parse_sdp_body(body)?;
        Some(Doc { origin: sdp_doc::parse_origin(body), doc })
    }

    /// The `m=` lines as `<media>/<transport>` pairs — the stream table a
    /// finding quotes. Empty where the description holds no `m=` line.
    fn stream_table(&self) -> Vec<String> {
        self.doc.media.iter().map(|m| format!("{}/{}", m.r#type, m.transport)).collect()
    }
}

/// One stream of one description as a report spells it: `<media> <port>
/// <proto>`, the port `?` where the m-line carried no readable one.
fn stream_row(m: &sip_message::sdp_doc::MediaLine) -> String {
    let port = m.port.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string());
    format!("{} {} {}", m.r#type, port, m.transport)
}

/// Whether the description a response carries BINDS as the round's answer on
/// its dialog: a final does (§13.2.1), and so does a reliable provisional — one
/// carrying BOTH `Require: 100rel` and an `RSeq` (RFC 3262 §5, the
/// [`super::prack`] reading). An unreliable provisional's description is early
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

/// The transport plan a description states — where the media goes and what
/// rides it: the session `c=` line, then one row per stream as `<media> <port>
/// <transport> <formats>` with the stream's own `c=` where it states one. The
/// `o=` line is deliberately absent: a plan re-stated under another origin is
/// [`SdpOriginContinuity`]'s occasion.
fn media_plan(d: &Doc) -> Vec<String> {
    let mut plan = vec![format!("c={}", d.doc.c_line.as_deref().unwrap_or(""))];
    plan.extend(d.doc.media.iter().map(|m| {
        let mut row = stream_row(m);
        for token in &m.formats {
            row.push(' ');
            row.push_str(token);
        }
        if let Some(c) = &m.c_line {
            row.push_str(" c=");
            row.push_str(c);
        }
        row
    }));
    plan
}

/// The finding shape every rule that judges an ANSWER produces: the answerer is
/// charged, on the offer's transaction, anchored at the answer it sent.
fn answer_finding(rule: RuleId, a: &Answered<'_>, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: a.src.to_string(),
        taker: a.dst.to_string(),
        cseq: a.offer_cseq,
        relayed: false, // a relay lane is the consumer's to skip
        anchor: a.msg,
        decision,
    }
}

/// One agent's offer/answer state on one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AgentKey<'a> {
    /// The endpoint whose state this is.
    agent: &'a str,
    call_id: &'a str,
}

/// The transaction an offer rode, as ONE agent sees it: the CSeq number, the
/// method, and whether THIS agent sent the request that carried it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RoundKey {
    cseq: u32,
    /// The method, ASCII-uppercased — the wire may spell it either way.
    method: String,
    request_sent_by_agent: bool,
}

/// One offer/answer round as one agent drove it.
#[derive(Debug)]
struct Round<'a> {
    /// Index into the view's `msgs` of the description that opened the round.
    offer: usize,
    /// The CSeq number of the transaction that offer rode.
    offer_cseq: u32,
    /// The agent SENT the description that opened this round, rather than
    /// taking it — the round is an offer of ITS OWN. Read by
    /// [`NoNewOfferWhileOfferPending`], which §5 charges only for overtaking
    /// the agent's own pending offer.
    offer_sent: bool,
    /// The early dialog the offer's own message named (its To tag), empty on an
    /// INITIAL request — which every dialog its forks open answers. Two early
    /// dialogs number their requests independently (§12.2.1.1), so an in-dialog
    /// round belongs to the one dialog whose tag it carries.
    offer_dialog: &'a str,
    /// Per early dialog (the To tag that tells two forks apart): the index into
    /// the view's `msgs` of the description that closed the round there.
    answered: BTreeMap<&'a str, usize>,
    /// Per early dialog: the index into the view's `msgs` of the description
    /// that stated the BINDING answer there — a final, or a reliable
    /// provisional (§13.2.1, RFC 3262 §5). An unreliable provisional's
    /// description is early media the peer may re-latch on and binds nothing,
    /// so it never enters here. Read by [`SecondAnswerRepeatsTheFirst`].
    bound: BTreeMap<&'a str, usize>,
}

/// What the vantage can say about the round an ACK acknowledges.
#[derive(Debug, Clone, Copy)]
enum AckRound {
    /// The vantage carried no body bytes for the ACK.
    BodyUnknown,
    /// The ACK carried no session description.
    NoDescription,
    /// The round already had both halves when the ACK went out.
    Closed { offer: usize, answer: usize },
    /// The round was still open — the ACK's description has a place in it.
    Open,
    /// The vantage never carried the round the ACK rides.
    RoundUnknown,
}

/// One ACK an agent sent, with the round it acknowledges.
#[derive(Debug)]
struct AckSighting<'a> {
    /// Index into the view's `msgs`.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    cseq: u32,
    round: AckRound,
}

/// One round an agent CLOSED with a description of its own.
#[derive(Debug)]
struct Answered<'a> {
    /// Index into the view's `msgs` of the closing description.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    /// Index into the view's `msgs` of the description that opened the round.
    offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    offer_cseq: u32,
}

/// One non-failure FINAL an agent sent on a round it took an offer for — the
/// last message §13.2.1 admits that round's answer in.
#[derive(Debug)]
struct AnswerDue<'a> {
    /// Index into the view's `msgs` of the final.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    status: u16,
    /// Index into the view's `msgs` of the description that opened the round.
    offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    offer_cseq: u32,
    /// Index into the view's `msgs` of the BINDING answer already on this
    /// dialog when the final went out — `None` where the final is the round's
    /// last occasion to state one.
    bound: Option<usize>,
}

/// One BINDING description an agent sent onto a dialog whose answer it had
/// ALREADY stated there — a second answer, which the peer ignores (§13.2.1).
#[derive(Debug)]
struct ReAnswer<'a> {
    /// Index into the view's `msgs` of the second description.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    /// Index into the view's `msgs` of the description that opened the round.
    offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    offer_cseq: u32,
    /// Index into the view's `msgs` of the answer that closed the round on this
    /// dialog — the one the peer is acting on.
    first: usize,
}

/// One description an agent SENT that OPENED a round of its own, with the state
/// of its own rounds AT THAT MOMENT — not at the end of the walk.
#[derive(Debug)]
struct OfferStep<'a> {
    /// Index into the view's `msgs`.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    cseq: u32,
    /// Index into the view's `msgs` of the agent's own most recent offer that
    /// had drawn no answer yet when this one went out — `None` where none was
    /// outstanding.
    pending: Option<usize>,
    /// The CSeq number of that pending offer's transaction.
    pending_cseq: u32,
}

/// One description an agent SENT, against the one it sent before on that call.
#[derive(Debug)]
struct OriginStep<'a> {
    /// Index into the view's `msgs`.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    cseq: u32,
    /// Index into the view's `msgs` of the agent's previous description with a
    /// readable `o=` line on this call — its origin stream's predecessor.
    prior: usize,
}

/// One agent's running state through the walk.
#[derive(Default)]
struct AgentState<'a> {
    rounds: Vec<Round<'a>>,
    /// Round index by the transaction that carried the offer.
    round_at: BTreeMap<RoundKey, usize>,
    /// Round index of a DELAYED offer, keyed by the INVITE CSeq, whether this
    /// agent sent that INVITE, and the early dialog the 2xx presented.
    delayed_at: BTreeMap<(u32, bool, &'a str), usize>,
    /// Index into the view's `msgs` of this agent's last SENT description with
    /// a readable `o=` line on this call.
    last_origin: Option<usize>,
    /// Indexes into the view's `msgs` of EVERY description this agent sent on
    /// this call, ascending — the stream [`Reading::sent_streams`] publishes.
    sent_docs: Vec<usize>,
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
struct Reading<'a> {
    docs: Vec<Option<Doc>>,
    acks: Vec<AckSighting<'a>>,
    answers: Vec<Answered<'a>>,
    origin_steps: Vec<OriginStep<'a>>,
    /// Descriptions an agent sent whose `o=` line no reader accepts, after its
    /// origin stream had already started.
    unreadable_origins: Vec<OriginStep<'a>>,
    /// Every offer an agent SENT, with what was pending when it went out. Read
    /// by [`NoNewOfferWhileOfferPending`].
    offer_steps: Vec<OfferStep<'a>>,
    /// Every non-failure final an agent sent on a round it took an offer for,
    /// in send order. Read by [`Final2xxAnswersTheOffer`].
    answers_due: Vec<AnswerDue<'a>>,
    /// Every binding description an agent sent onto a dialog whose answer it
    /// had already stated, in send order. Read by
    /// [`SecondAnswerRepeatsTheFirst`].
    re_answers: Vec<ReAnswer<'a>>,
    /// Per agent and call, the indexes into the view's `msgs` of the
    /// descriptions that agent SENT, ascending. Read by
    /// [`ReOfferMLineCountMonotonic`] (consecutive pairs) and
    /// [`PayloadTypeMappingStable`] (the payload-type bindings the whole stream
    /// states).
    sent_streams: Vec<Vec<usize>>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
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
    fn round_docs(&self, a: &Answered<'_>) -> Option<(&Doc, &Doc)> {
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
                        if !has_doc
                            || *status < 200
                            || !msg.cseq_method.eq_ignore_ascii_case("INVITE")
                        {
                            return;
                        }
                        let slot = (msg.cseq, request_sent_by_agent, dialog);
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

// ---------------------------------------------------------------------------
// ack-body-after-complete-offer-answer
// ---------------------------------------------------------------------------

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
    fn id(&self) -> RuleId {
        RuleId::AckBodyAfterCompleteOfferAnswer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for ack in &seen.acks {
            let head = |decision| Finding {
                rule: RuleId::AckBodyAfterCompleteOfferAnswer,
                emitter: ack.src.to_string(),
                taker: ack.dst.to_string(),
                cseq: ack.cseq,
                relayed: false, // a relay lane is the consumer's to skip
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

// ---------------------------------------------------------------------------
// final-2xx-answers-the-offer
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.2.1 / RFC 3264 §6 — an offer is ANSWERED.** The answer to an
/// offer a request carried rides a reliable non-failure message correlated to
/// that request, and the 2xx is the last one there is: a 2xx carrying no
/// description over an offer no reliable message has answered ends the round
/// with the two ends holding different media plans, and the peer has nothing
/// left to latch on.
///
/// The occasion is a 2xx an agent sent on a transaction whose request it took
/// an offer on — `INVITE` or `UPDATE` ([`offer_answer_method`]) — and on the
/// dialog that offer named, since two early dialogs number their requests
/// independently (§12.2.1.1). It is met by a description on that 2xx, or by one
/// that already BOUND the dialog ([`binds`]): a reliable provisional's answer
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
    fn id(&self) -> RuleId {
        RuleId::Final2xxAnswersTheOffer
    }

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
                relayed: false, // a relay lane is the consumer's to skip
                anchor: due.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// second-answer-repeats-the-first
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.2.1 — one dialog carries ONE answer.** The peer treats the
/// answer it receives as the answer and ignores every later description on that
/// dialog, so a second one there either re-states the same plan (which §13.2.1
/// admits) or states a plan only its sender is acting on, leaving the two ends
/// sending to different places.
///
/// The occasion is a description that BINDS — a final, or a reliable
/// provisional ([`binds`]) — on a dialog where one already did. Early media in
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
/// another origin is [`SdpOriginContinuity`]'s occasion, and a second
/// description an ACK carries is [`AckBodyAfterCompleteOfferAnswer`]'s.
pub struct SecondAnswerRepeatsTheFirst;

impl Obligation for SecondAnswerRepeatsTheFirst {
    fn id(&self) -> RuleId {
        RuleId::SecondAnswerRepeatsTheFirst
    }

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
                relayed: false, // a relay lane is the consumer's to skip
                anchor: r.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// answer-stream-matches-offer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — an accepted stream keeps the offer's media type AND
/// transport protocol.** The answer holds one `m=` line per offered stream, in
/// the offer's order; the `m=` line the answerer sends back for stream *i*
/// re-states that stream's `<media>` and `<proto>` — rejecting it means port 0,
/// never re-typing it. A re-typed or re-transported stream leaves the two ends
/// sending incompatible media on the same slot.
///
/// Charges the ANSWERER, on every round it closes and not only the first. One
/// finding per answer, naming every stream it re-typed: the answer is one act,
/// and a table of streams is one disagreement about that table (the
/// [`Evidence::MultipleFinals`] precedent).
pub struct AnswerStreamMatchesOffer;

impl Obligation for AnswerStreamMatchesOffer {
    fn id(&self) -> RuleId {
        RuleId::AnswerStreamMatchesOffer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered = Vec::new();
            let mut answered = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.r#type == n.r#type && o.transport == n.transport {
                    continue;
                }
                stream_indexes.push(i);
                offered.push(stream_row(o));
                answered.push(stream_row(n));
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerStreamRetyped {
                    answer_stream_msg: a.msg,
                    answer_stream_hop: wire.msgs[a.msg].hop,
                    answer_stream_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered,
                    answered,
                })
            };
            out.push(answer_finding(RuleId::AnswerStreamMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// sdp-origin-continuity
// ---------------------------------------------------------------------------

/// **RFC 4566 §5.2 / RFC 3264 §8 — every session description an agent sends on
/// one call describes the SAME session, and its `sess-version` counts the
/// revisions.** `o=<username> <sess-id> <sess-version> <nettype> <addrtype>
/// <address>` identifies it: a later description repeats all five identity
/// fields, raises `sess-version` by exactly one when the rest of the
/// description changed, and leaves it alone when nothing did. A changed
/// identity makes the description a different session the peer must treat as
/// unrelated; a version that does not track the changes leaves the peer unable
/// to tell a re-offer from a repeat.
///
/// Charges the description's SENDER, against its own previous description on
/// that call. A description whose `o=` line no reader accepts settles nothing —
/// the stream's predecessor is the last one that did.
pub struct SdpOriginContinuity;

impl Obligation for SdpOriginContinuity {
    fn id(&self) -> RuleId {
        RuleId::SdpOriginContinuity
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        let head = |s: &OriginStep<'_>, decision| Finding {
            rule: RuleId::SdpOriginContinuity,
            emitter: s.src.to_string(),
            taker: s.dst.to_string(),
            cseq: s.cseq,
            relayed: false,
            anchor: s.msg,
            decision,
        };
        for s in &seen.unreadable_origins {
            out.push(head(s, Decision::Undecidable("the description carries no readable o= line")));
        }
        for s in &seen.origin_steps {
            let (Some(now), Some(before)) = (
                seen.docs[s.msg].as_ref().and_then(|d| d.origin.as_ref()),
                seen.docs[s.prior].as_ref().and_then(|d| d.origin.as_ref()),
            ) else {
                continue;
            };
            let same_session = before.identifies_same_session(now);
            let body_changed = before.body_excluding_origin != now.body_excluding_origin;
            let delta = i128::from(now.session_version) - i128::from(before.session_version);
            let ok = same_session && delta == i128::from(body_changed);
            let decision = if ok {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::SdpOriginDiverged {
                    origin_msg: s.msg,
                    origin_hop: wire.msgs[s.msg].hop,
                    origin_ts_us: wire.msgs[s.msg].at_us,
                    prior_origin_msg: s.prior,
                    origin_line: now.raw_origin_line.clone(),
                    prior_origin_line: before.raw_origin_line.clone(),
                    same_session,
                    body_changed,
                    session_version: now.session_version,
                    prior_session_version: before.session_version,
                })
            };
            out.push(head(s, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// no-new-offer-while-offer-pending
// ---------------------------------------------------------------------------

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
    fn id(&self) -> RuleId {
        RuleId::NoNewOfferWhileOfferPending
    }

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
                relayed: false, // a relay lane is the consumer's to skip
                anchor: s.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// answer-m-line-count-matches-offer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — an answer holds exactly one `m=` line per offered stream.**
/// The two ends index streams by position, so a rejected stream keeps its slot
/// at port 0 and an added one is a re-offer's business, never an answer's. A
/// count that differs desyncs the stream table: from that position on, the two
/// ends mean different streams by the same index.
///
/// Charges the ANSWERER, on every round it closes and not only the first.
pub struct AnswerMLineCountMatchesOffer;

impl Obligation for AnswerMLineCountMatchesOffer {
    fn id(&self) -> RuleId {
        RuleId::AnswerMLineCountMatchesOffer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let (offer_m_lines, answer_m_lines) = (offer.doc.media.len(), answer.doc.media.len());
            let decision = if offer_m_lines == answer_m_lines {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerMLineCountDiffers {
                    m_count_msg: a.msg,
                    m_count_hop: wire.msgs[a.msg].hop,
                    m_count_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    offer_m_lines,
                    answer_m_lines,
                    offered_streams: offer.stream_table(),
                    answered_streams: answer.stream_table(),
                })
            };
            out.push(answer_finding(RuleId::AnswerMLineCountMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// answer-t-line-equals-offer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — an answer repeats the offer's `t=` line.** The session's
/// time bounds are the offerer's statement and the answer carries them back
/// verbatim; a rewritten `t=` leaves the two ends disagreeing on when the
/// session is active. A description carrying no `t=` line differs from one that
/// carries any.
///
/// Charges the ANSWERER, on every round it closes.
pub struct AnswerTLineEqualsOffer;

impl Obligation for AnswerTLineEqualsOffer {
    fn id(&self) -> RuleId {
        RuleId::AnswerTLineEqualsOffer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let decision = if offer.doc.t_line == answer.doc.t_line {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerTLineDiffers {
                    t_line_msg: a.msg,
                    t_line_hop: wire.msgs[a.msg].hop,
                    t_line_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    offer_t_line: offer.doc.t_line.clone().unwrap_or_default(),
                    answer_t_line: answer.doc.t_line.clone().unwrap_or_default(),
                })
            };
            out.push(answer_finding(RuleId::AnswerTLineEqualsOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// answer-media-type-matches-offer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6.1 — the answer's stream at each offered position states that
/// position's media type.** Streams pair by index, so an answer that puts video
/// where audio was offered breaks the correlation both ends apply the
/// negotiated media by.
///
/// Charges the ANSWERER, over the positions both descriptions hold — a count
/// mismatch beyond that is [`AnswerMLineCountMatchesOffer`]'s occasion. One
/// finding per answer, naming every mis-typed position: the answer is one act
/// (the [`Evidence::MultipleFinals`] precedent).
pub struct AnswerMediaTypeMatchesOffer;

impl Obligation for AnswerMediaTypeMatchesOffer {
    fn id(&self) -> RuleId {
        RuleId::AnswerMediaTypeMatchesOffer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered_types = Vec::new();
            let mut answered_types = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.r#type == n.r#type {
                    continue;
                }
                stream_indexes.push(i);
                offered_types.push(o.r#type.clone());
                answered_types.push(n.r#type.clone());
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::AnswerMediaTypeMismatched {
                    media_type_msg: a.msg,
                    media_type_hop: wire.msgs[a.msg].hop,
                    media_type_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered_types,
                    answered_types,
                })
            };
            out.push(answer_finding(RuleId::AnswerMediaTypeMatchesOffer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// direction-pair-valid
// ---------------------------------------------------------------------------

/// The answer directions RFC 3264 §6.1 admits for an offered one:
/// `sendonly→{recvonly,inactive}`, `recvonly→{sendonly,inactive}`,
/// `inactive→{inactive}`, `sendrecv→{sendrecv,sendonly,recvonly,inactive}`.
fn answer_valid_for_offer(offer: SdpDirection, answer: SdpDirection) -> bool {
    use SdpDirection::*;
    match offer {
        SendOnly => matches!(answer, RecvOnly | Inactive),
        RecvOnly => matches!(answer, SendOnly | Inactive),
        Inactive => matches!(answer, Inactive),
        SendRecv => matches!(answer, SendRecv | SendOnly | RecvOnly | Inactive),
    }
}

/// **RFC 3264 §6.1 — an answer's direction attribute is one the offer's
/// admits.** The pair states who sends: an answer of `sendonly` to `sendonly`
/// leaves both ends transmitting and neither receiving, and an answer of
/// anything but `inactive` to `inactive` claims a stream the offerer disabled.
/// An `m=` block carrying no direction attribute states `sendrecv`, the default
/// §6.1 defines — an absence, never an unknown.
///
/// Charges the ANSWERER, over the positions both descriptions hold. One finding
/// per answer, naming every mis-paired position.
pub struct DirectionPairValid;

impl Obligation for DirectionPairValid {
    fn id(&self) -> RuleId {
        RuleId::DirectionPairValid
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut offered_directions = Vec::new();
            let mut answered_directions = Vec::new();
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                let (od, nd) = (sdp_doc::extract_direction(o), sdp_doc::extract_direction(n));
                if answer_valid_for_offer(od, nd) {
                    continue;
                }
                stream_indexes.push(i);
                offered_directions.push(od.token().to_string());
                answered_directions.push(nd.token().to_string());
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::DirectionPairInvalid {
                    direction_msg: a.msg,
                    direction_hop: wire.msgs[a.msg].hop,
                    direction_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    offered_directions,
                    answered_directions,
                })
            };
            out.push(answer_finding(RuleId::DirectionPairValid, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// rejected-stream-minimal-answer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — an answer rejecting a stream still lists a media format.**
/// Rejection is port 0 on an otherwise complete `m=` line: the format list is
/// part of the line's grammar (RFC 4566 §5.14), so a bare `m=audio 0 RTP/AVP`
/// is a description a strict peer refuses rather than the disable it meant.
///
/// Charges the ANSWERER, on every round it closes; the offer decides nothing
/// here, so the answer alone is the occasion. One finding per answer, naming
/// every bare rejection.
pub struct RejectedStreamMinimalAnswer;

impl Obligation for RejectedStreamMinimalAnswer {
    fn id(&self) -> RuleId {
        RuleId::RejectedStreamMinimalAnswer
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((_, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut rejected_rows = Vec::new();
            for (i, m) in answer.doc.media.iter().enumerate() {
                if m.port != Some(0) || !m.formats.is_empty() {
                    continue;
                }
                stream_indexes.push(i);
                rejected_rows.push(stream_row(m));
            }
            let decision = if stream_indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::RejectedStreamWithoutFormat {
                    rejected_stream_msg: a.msg,
                    rejected_stream_hop: wire.msgs[a.msg].hop,
                    rejected_stream_ts_us: wire.msgs[a.msg].at_us,
                    stream_indexes,
                    rejected_rows,
                })
            };
            out.push(answer_finding(RuleId::RejectedStreamMinimalAnswer, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// re-offer-m-line-count-monotonic
// ---------------------------------------------------------------------------

/// **RFC 3264 §8 — an agent's later description carries at least as many `m=`
/// lines as the one before it.** A stream is removed by keeping its slot at
/// port 0, and a new one is appended BELOW the existing ones, so the stream
/// table only ever grows. A shorter table renumbers every stream below the
/// dropped one, and the peer applies the answer to the wrong streams.
///
/// Charges the description's SENDER, against its own previous description on
/// that call — answers included, since the table is the agent's whatever
/// message carries it. A description this vantage carried no body for is no
/// step of the stream at all.
pub struct ReOfferMLineCountMonotonic;

impl Obligation for ReOfferMLineCountMonotonic {
    fn id(&self) -> RuleId {
        RuleId::ReOfferMLineCountMonotonic
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for stream in &seen.sent_streams {
            for step in stream.windows(2) {
                let (prior, msg) = (step[0], step[1]);
                let (Some(before), Some(now)) =
                    (seen.docs[prior].as_ref(), seen.docs[msg].as_ref())
                else {
                    continue;
                };
                let (prior_m_lines, m_lines) = (before.doc.media.len(), now.doc.media.len());
                let decision = if m_lines >= prior_m_lines {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::ReOfferStreamsDropped {
                        re_offer_msg: msg,
                        re_offer_hop: wire.msgs[msg].hop,
                        re_offer_ts_us: wire.msgs[msg].at_us,
                        prior_offer_msg: prior,
                        m_lines,
                        prior_m_lines,
                    })
                };
                out.push(Finding {
                    rule: RuleId::ReOfferMLineCountMonotonic,
                    emitter: wire.msgs[msg].src.clone(),
                    taker: wire.msgs[msg].dst.clone(),
                    cseq: wire.msgs[msg].cseq,
                    relayed: false, // a relay lane is the consumer's to skip
                    anchor: msg,
                    decision,
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// zero-port-propagation
// ---------------------------------------------------------------------------

/// **RFC 3264 §8 — a stream offered at port 0 is answered at port 0.** Port 0
/// is the offerer disabling the stream; the answerer cannot revive it, and a
/// live port there points the offerer's media at a stream it just took down.
///
/// Charges the ANSWERER, over the positions both descriptions hold. An answer
/// `m=` line carrying no readable port states no assignment, so that position
/// settles nothing — an answer whose every disabled position is unreadable is
/// undecidable rather than clean. One finding per answer, naming every revived
/// position.
pub struct ZeroPortPropagation;

impl Obligation for ZeroPortPropagation {
    fn id(&self) -> RuleId {
        RuleId::ZeroPortPropagation
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for a in &seen.answers {
            let Some((offer, answer)) = seen.round_docs(a) else { continue };
            let mut stream_indexes = Vec::new();
            let mut answered_ports = Vec::new();
            let (mut disabled, mut unreadable) = (0u32, 0u32);
            for (i, (o, n)) in offer.doc.media.iter().zip(answer.doc.media.iter()).enumerate() {
                if o.port != Some(0) {
                    continue;
                }
                disabled += 1;
                match n.port {
                    None => unreadable += 1,
                    Some(0) => {}
                    Some(p) => {
                        stream_indexes.push(i);
                        answered_ports.push(p);
                    }
                }
            }
            let decision = if !stream_indexes.is_empty() {
                Decision::Violated(Evidence::ZeroPortResurrected {
                    zero_port_msg: a.msg,
                    zero_port_hop: wire.msgs[a.msg].hop,
                    zero_port_ts_us: wire.msgs[a.msg].at_us,
                    offer_msg: a.offer,
                    stream_indexes,
                    answered_ports,
                })
            } else if disabled > 0 && unreadable == disabled {
                Decision::Undecidable(
                    "the answer states no readable port for any stream the offer disabled",
                )
            } else {
                Decision::Compliant
            };
            out.push(answer_finding(RuleId::ZeroPortPropagation, a, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// payload-type-mapping-stable
// ---------------------------------------------------------------------------

/// True iff two `a=rtpmap` encodings name ONE binding, read as the media type
/// under judgement spells them: the case-insensitive MIME subtype comparison of
/// RFC 4566 §6, over the canonical form [`sdp_doc::canonical_rtpmap`] gives each.
fn same_binding(media_type: &str, one: &str, other: &str) -> bool {
    sdp_doc::canonical_rtpmap(media_type, one)
        .eq_ignore_ascii_case(&sdp_doc::canonical_rtpmap(media_type, other))
}

/// **RFC 3264 §8.3.2 — a payload type an agent has bound to an encoding keeps
/// that encoding for the whole call.** The peer caches `a=rtpmap:<pt> <enc>`
/// and decodes by the cached binding, so re-binding a payload type — in a later
/// description or in a further `m=` block of the same one — decodes the
/// sender's media as the wrong codec. Two spellings of one binding are not a
/// re-bind (RFC 4566 §6): encoding names are MIME subtypes and compare
/// case-insensitively, and on an audio stream an absent channel count is one,
/// so `pcma`/`PCMA` and `PCMA/8000`/`PCMA/8000/1` each name a single binding.
/// The finding quotes the encodings the wire stated.
///
/// Charges the description's SENDER, against the bindings its own earlier
/// descriptions on that call stated. Every description it sends is an occasion,
/// the first included: the binding a description makes in one `m=` block binds
/// the rest of it too. One finding per description, naming every re-bound
/// payload type.
pub struct PayloadTypeMappingStable;

impl Obligation for PayloadTypeMappingStable {
    fn id(&self) -> RuleId {
        RuleId::PayloadTypeMappingStable
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for stream in &seen.sent_streams {
            // Payload type → the encoding text this agent first bound it to.
            let mut bound: Vec<(String, String)> = Vec::new();
            for &mi in stream {
                let Some(doc) = seen.docs[mi].as_ref() else { continue };
                let mut payload_types = Vec::new();
                let mut prior_encodings = Vec::new();
                let mut encodings = Vec::new();
                for media in &doc.doc.media {
                    for (pt, enc) in sdp_doc::extract_rtpmaps(media) {
                        let first = bound.iter().find(|(p, _)| *p == pt).map(|(_, e)| e.clone());
                        match first {
                            None => bound.push((pt, enc)),
                            Some(prev) if !same_binding(&media.r#type, &prev, &enc) => {
                                payload_types.push(pt);
                                prior_encodings.push(prev);
                                encodings.push(enc);
                            }
                            Some(_) => {}
                        }
                    }
                }
                let decision = if payload_types.is_empty() {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::PayloadTypeRemapped {
                        payload_type_msg: mi,
                        payload_type_hop: wire.msgs[mi].hop,
                        payload_type_ts_us: wire.msgs[mi].at_us,
                        payload_types,
                        prior_encodings,
                        encodings,
                    })
                };
                out.push(Finding {
                    rule: RuleId::PayloadTypeMappingStable,
                    emitter: wire.msgs[mi].src.clone(),
                    taker: wire.msgs[mi].dst.clone(),
                    cseq: wire.msgs[mi].cseq,
                    relayed: false, // a relay lane is the consumer's to skip
                    anchor: mi,
                    decision,
                });
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

// ---------------------------------------------------------------------------
// The two obligations a description owes ON ITS OWN
// ---------------------------------------------------------------------------
//
// Neither needs the round walk above: a description that no reader accepts, and
// one that states two dispositions for one stream, are wrong before any peer
// answers them. They stay in this file because RFC 3264 is one family and every
// `sdp_doc` reading in this crate lives here — and, like every rule above, each
// judges what an agent SENT.

/// **RFC 3264 §5-6 / RFC 4566 §5 — a body sent as SDP satisfies the
/// offer/answer grammar.** A real peer 488s a malformed description: no `v=0`
/// session, two sessions in one body, an `o=` sess-id/version that is not a
/// digit string, a stream with no `c=` or no port, a non-positive `a=ptime`. A test UA accepts the body and masks the defect, so
/// the recording is where the offer's own grammar is checked at all.
///
/// **The occasion is one fresh message the endpoint sent DECLARING a
/// description and carrying bytes** — `application/sdp`, or a `multipart/…`
/// body framing an SDP part (RFC 5621 §3.1). The declaration is what says
/// these bytes are a description: a body under another Content-Type is not this
/// rule's to read, and a vantage carrying no head or no body opens no occasion
/// rather than guessing at one. The grammar walk is
/// [`sip_message::sdp::validate_offer_answer_body`]'s — this rule reads a
/// description, it never parses one. Charges the sender that minted the body.
pub struct SdpBodyParseable;

impl Obligation for SdpBodyParseable {
    fn id(&self) -> RuleId {
        RuleId::SdpBodyParseable
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(body) = msg.sdp() else { continue };
            let decision = match sdp::validate_offer_answer_body(body) {
                Ok(()) => Decision::Compliant,
                Err(e) => Decision::Violated(Evidence::SdpBodyRejected {
                    sdp_body_msg: mi,
                    sdp_body_hop: msg.hop,
                    sdp_body_ts_us: msg.at_us,
                    sdp_failure: e.reason,
                }),
            };
            out.push(Finding {
                rule: RuleId::SdpBodyParseable,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}

/// **RFC 3264 §6 / §8.4 — a stream is held OR rejected, never both.** Setting
/// the connection address to the unspecified one (`c=IN IP4 0.0.0.0`, the
/// legacy hold idiom) says "keep the stream, send nothing there"; port 0 says
/// "this stream is gone". A description stating both leaves the peer no
/// disposition it can act on, and the two answers it could give differ.
///
/// **The occasion is one fresh description the endpoint sent**, whatever
/// Content-Type declared it — the shape alone identifies a session description,
/// and this rule reads the disposition rather than the format. The applicable
/// `c=` is the stream's own where it states one, the session's otherwise
/// (§5.7), so a block overriding an unspecified session address with a real one
/// is not held at all.
///
/// **One finding per DESCRIPTION**, naming every stream at fault: writing one
/// body with contradictory dispositions is one act. Charges the sender; scoped
/// to the offering UAC by consumer policy, since an answerer legitimately
/// rejects a stream at port 0 while echoing a real address.
pub struct C0PortNonZero;

impl Obligation for C0PortNonZero {
    fn id(&self) -> RuleId {
        RuleId::C0PortNonZero
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(carried) = Doc::of(msg) else { continue };
            let session_c = carried.doc.c_line.as_deref();
            let session_held = session_c.is_some_and(sdp::c_line_is_unspecified);
            let (mut indexes, mut streams, mut c_lines) = (Vec::new(), Vec::new(), Vec::new());
            for (i, media) in carried.doc.media.iter().enumerate() {
                let applicable = media.c_line.as_deref().or(session_c);
                let held = match media.c_line.as_deref() {
                    Some(own) => sdp::c_line_is_unspecified(own),
                    None => session_held,
                };
                if held && media.port == Some(0) {
                    indexes.push(i);
                    streams.push(stream_row(media));
                    c_lines.push(applicable.unwrap_or_default().to_string());
                }
            }
            let decision = if indexes.is_empty() {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::HeldAndRejectedStreams {
                    held_stream_msg: mi,
                    held_stream_hop: msg.hop,
                    held_stream_ts_us: msg.at_us,
                    held_stream_indexes: indexes,
                    held_streams: streams,
                    held_c_lines: c_lines,
                })
            };
            out.push(Finding {
                rule: RuleId::C0PortNonZero,
                emitter: msg.src.clone(),
                taker: msg.dst.clone(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Observation;

    const ALICE: &str = "10.0.0.1:5060";
    const BOB: &str = "10.0.0.2:5070";

    const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

    const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

    const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

    const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

    /// The harness default body that must never ride an ACK: a fresh session
    /// (`o=bob 1 1`) offering audio over RTP/AVP.
    const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

    /// The head a synthetic message carries: `extra` lines, and the
    /// `Content-Type` a body demands (RFC 3261 §20.15) — `application/sdp`,
    /// which is what every description these tests carry declares.
    fn head(extra: &str, body: Option<&str>) -> Vec<u8> {
        let declared = match body {
            Some(b) if !b.is_empty() => "Content-Type: application/sdp\r\n",
            _ => "",
        };
        format!("X: y\r\n{extra}{declared}\r\n").into_bytes()
    }

    fn req(
        at_us: u64,
        src: &str,
        dst: &str,
        method: &str,
        cseq: u32,
        to_tag: Option<&str>,
        body: Option<&str>,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "cid-1@host".to_string(),
            cseq,
            cseq_method: method.to_string(),
            via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
            from_tag: Some("at".to_string()),
            to_tag: to_tag.map(str::to_string),
            head: Some(head("", body)),
            body: Some(body.unwrap_or("").as_bytes().to_vec()),
        }
    }

    fn resp(
        at_us: u64,
        src: &str,
        dst: &str,
        status: u16,
        cseq: u32,
        method: &str,
        body: Option<&str>,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "cid-1@host".to_string(),
            cseq,
            cseq_method: method.to_string(),
            via_branch: Some(format!("z9hG4bK-{method}-{cseq}")),
            from_tag: Some("at".to_string()),
            to_tag: Some("bt".to_string()),
            head: Some(head("", body)),
            body: Some(body.unwrap_or("").as_bytes().to_vec()),
        }
    }

    fn view(msgs: &[Msg]) -> (Observation, &[Msg]) {
        (Observation { closed: true, ..Observation::default() }, msgs)
    }

    fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        let (obs, msgs) = view(msgs);
        rule.eval(&WireView { msgs, obs: &obs })
    }

    fn violations(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        run(rule, msgs).into_iter().filter(Finding::violated).collect()
    }

    /// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
    /// 200(answer) → ACK. The ACK bodies are the caller's choice.
    fn t38_call(first_ack: Option<&str>, second_ack: Option<&str>) -> Vec<Msg> {
        vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 180, 1, "INVITE", Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", None),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), first_ack),
            req(5, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
            resp(6, BOB, ALICE, 200, 3, "INVITE", Some(T38_ANSWER)),
            req(7, ALICE, BOB, "ACK", 3, Some("bt"), second_ack),
        ]
    }

    // ---- ack-body-after-complete-offer-answer --------------------------

    #[test]
    fn ack_with_no_body_after_a_complete_round_is_clean() {
        let msgs = t38_call(None, None);
        let out = run(&AckBodyAfterCompleteOfferAnswer, &msgs);
        assert_eq!(out.len(), 2, "one occasion per ACK: {out:?}");
        assert!(out.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{out:?}");
    }

    #[test]
    fn ack_carrying_a_body_on_an_answered_round_is_flagged() {
        let msgs = t38_call(None, Some(STRAY_ANSWER));
        let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, ALICE);
        assert_eq!(f[0].taker, BOB);
        assert_eq!(f[0].cseq, 3);
        let Decision::Violated(Evidence::AckBodyOnClosedRound {
            offer_msg,
            answer_msg,
            streams,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(*offer_msg, 4, "the T.38 re-INVITE carried the offer");
        assert_eq!(*answer_msg, 5, "its 200 carried the answer");
        assert_eq!(streams, &vec!["audio/RTP/AVP".to_string()]);
    }

    #[test]
    fn ack_body_on_the_establishing_round_answered_by_a_180_is_flagged() {
        let msgs = t38_call(Some(STRAY_ANSWER), None);
        let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::AckBodyOnClosedRound { answer_msg, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(*answer_msg, 1, "the 180 closed the round, not the bodyless 200");
    }

    #[test]
    fn delayed_offer_ack_carrying_the_answer_is_clean() {
        // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, None),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
        ];
        assert!(violations(&AckBodyAfterCompleteOfferAnswer, &msgs).is_empty());
    }

    #[test]
    fn an_ack_from_a_body_less_vantage_is_undecidable() {
        let mut msgs = t38_call(None, Some(STRAY_ANSWER));
        for m in &mut msgs {
            m.body = None;
        }
        let out = run(&AckBodyAfterCompleteOfferAnswer, &msgs);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| !f.decided()), "{out:?}");
    }

    #[test]
    fn a_retransmitted_offending_ack_is_one_occasion() {
        let mut msgs = t38_call(None, Some(STRAY_ANSWER));
        let mut again = msgs[6].clone();
        again.at_us = 8;
        again.repeat = true;
        msgs.push(again);
        let f = violations(&AckBodyAfterCompleteOfferAnswer, &msgs);
        assert_eq!(f.len(), 1, "a retransmission is not a second ACK: {f:?}");
    }

    // ---- final-2xx-answers-the-offer ------------------------------------

    /// A reliable provisional: `Require: 100rel` and an `RSeq`, so its
    /// description BINDS (RFC 3262 §5).
    fn reliable_1xx(at_us: u64, status: u16, cseq: u32, body: Option<&str>) -> Msg {
        Msg {
            head: Some(head("Require: 100rel\r\nRSeq: 1\r\n", body)),
            ..resp(at_us, BOB, ALICE, status, cseq, "INVITE", body)
        }
    }

    #[test]
    fn a_2xx_carrying_the_answer_meets_the_offer() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(out.len(), 1, "one occasion, the 2xx: {out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
    }

    /// RFC 5621 §3.1: the answer rides inside a `multipart/mixed` body beside
    /// another part, and the reading takes the SDP part as the description.
    #[test]
    fn an_answer_framed_in_a_multipart_body_meets_the_offer() {
        use sip_message::{compose_multipart, MultipartPart};
        let framed = compose_multipart(
            "multipart/mixed",
            &[
                MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15]),
                MultipartPart::new("application/sdp", AUDIO_ANSWER.as_bytes().to_vec()),
            ],
        )
        .unwrap();
        let answer = Msg {
            head: Some(
                format!("X: y\r\nContent-Type: {}\r\n\r\n", framed.content_type).into_bytes(),
            ),
            body: Some(framed.body),
            ..resp(2, BOB, ALICE, 200, 1, "INVITE", None)
        };
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            answer,
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(out.len(), 1, "one occasion, the 2xx: {out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
        let parse = run(&SdpBodyParseable, &msgs);
        assert!(parse.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{parse:?}");
    }

    #[test]
    fn a_bodiless_2xx_leaves_the_offer_unanswered() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", None),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, BOB, "the answerer is charged");
        assert_eq!(f[0].taker, ALICE);
        assert_eq!(f[0].cseq, 1);
        let Decision::Violated(Evidence::OfferLeftUnanswered {
            unanswered_final_msg,
            status,
            offer_msg,
            offered_streams,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(*unanswered_final_msg, 1);
        assert_eq!(*status, 200);
        assert_eq!(*offer_msg, 0);
        assert_eq!(offered_streams, &vec!["audio/RTP/AVP".to_string()]);
    }

    #[test]
    fn a_reliable_provisional_answer_discharges_the_2xx() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            reliable_1xx(2, 183, 1, Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", None),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(out.len(), 1, "the reliable 183 is no occasion of its own: {out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
    }

    #[test]
    fn early_media_in_an_unreliable_provisional_answers_nothing() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 183, 1, "INVITE", Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", None),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "a plan the peer may re-latch on is no answer: {f:?}");
    }

    #[test]
    fn a_failure_final_owes_no_answer() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 486, 1, "INVITE", None),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
    }

    #[test]
    fn a_cancelled_round_is_no_occasion() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 180, 1, "INVITE", None),
            req(3, ALICE, BOB, "CANCEL", 1, None, None),
            resp(4, BOB, ALICE, 200, 1, "CANCEL", None),
            resp(5, BOB, ALICE, 487, 1, "INVITE", None),
            req(6, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
    }

    #[test]
    fn a_round_the_recording_cuts_off_is_no_occasion() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 180, 1, "INVITE", None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "no answer YET is no absence");
    }

    #[test]
    fn a_delayed_offer_answered_in_the_ack_is_no_occasion() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, None),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "the ACK answers, not the 2xx");
    }

    #[test]
    fn a_bodiless_2xx_to_a_bodiless_invite_is_no_occasion() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, None),
            resp(2, BOB, ALICE, 200, 1, "INVITE", None),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
    }

    #[test]
    fn a_re_invite_owes_its_own_answer() {
        let msgs = t38_call(None, None);
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "the establishing round's bodiless 200: {f:?}");
        assert_eq!(f[0].cseq, 1);
        let mut cut = t38_call(None, None);
        cut[5] = resp(6, BOB, ALICE, 200, 3, "INVITE", None);
        let f = violations(&Final2xxAnswersTheOffer, &cut);
        assert_eq!(f.len(), 2, "the T.38 re-offer owes an answer too: {f:?}");
        assert_eq!(f[1].cseq, 3);
    }

    #[test]
    fn an_update_owes_its_own_answer() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
            req(4, ALICE, BOB, "UPDATE", 2, Some("bt"), Some(T38_OFFER)),
            resp(5, BOB, ALICE, 200, 2, "UPDATE", None),
        ];
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].cseq, 2);
    }

    #[test]
    fn a_prack_answering_a_reliable_1xx_offer_is_no_occasion() {
        // RFC 3262 §5's delayed-offer form: the offer rides the reliable 183,
        // the PRACK carries the answer, and the 200 to the PRACK owes nothing.
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, None),
            reliable_1xx(2, 183, 1, Some(AUDIO_OFFER)),
            req(3, ALICE, BOB, "PRACK", 2, Some("bt"), Some(AUDIO_ANSWER)),
            resp(4, BOB, ALICE, 200, 2, "PRACK", None),
            resp(5, BOB, ALICE, 200, 1, "INVITE", None),
            req(6, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
    }

    #[test]
    fn two_early_dialogs_sharing_a_cseq_are_two_rounds() {
        // Each early dialog numbers its own requests (§12.2.1.1), so fork 1's
        // bodiless UPDATE and fork 2's re-offer both ride CSeq 3. Only fork 2
        // opened a round, and fork 1's 200 answers nothing it took.
        let tagged = |m: Msg, tag: &str| Msg { to_tag: Some(tag.to_string()), ..m };
        let msgs = vec![
            tagged(req(1, ALICE, BOB, "UPDATE", 3, Some("f2"), Some(T38_OFFER)), "f2"),
            tagged(resp(2, BOB, ALICE, 200, 3, "UPDATE", Some(T38_ANSWER)), "f2"),
            tagged(req(3, ALICE, BOB, "UPDATE", 3, Some("f1"), None), "f1"),
            tagged(resp(4, BOB, ALICE, 200, 3, "UPDATE", None), "f1"),
        ];
        let out = run(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(out.len(), 1, "only fork 2 carried an offer: {out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant), "{out:?}");
    }

    #[test]
    fn an_options_capability_body_is_no_offer() {
        let msgs = vec![
            req(1, ALICE, BOB, "OPTIONS", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "OPTIONS", None),
        ];
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty(), "§11.2 is not a round");
    }

    #[test]
    fn a_final_from_a_body_less_vantage_settles_nothing() {
        let mut msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        ];
        for m in &mut msgs {
            m.body = None;
        }
        // A body-less vantage carries no offer either, so the round never opens.
        assert!(run(&Final2xxAnswersTheOffer, &msgs).is_empty());
        let mut half = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        ];
        half[1].body = None;
        let out = run(&Final2xxAnswersTheOffer, &half);
        assert_eq!(out.len(), 1);
        assert!(!out[0].decided(), "{out:?}");
    }

    #[test]
    fn a_final_carrying_bytes_that_are_no_description_settles_nothing() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some("--b\r\nContent-Type: x\r\n")),
        ];
        let out = run(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(out.len(), 1);
        assert!(!out[0].decided(), "a multipart the reading cannot open is no proof: {out:?}");
    }

    #[test]
    fn a_retransmitted_bodiless_final_is_one_occasion() {
        let mut msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", None),
        ];
        let mut again = msgs[1].clone();
        again.at_us = 3;
        again.repeat = true;
        msgs.push(again);
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "a retransmission is not a second final: {f:?}");
    }

    #[test]
    fn two_forks_each_owe_their_own_dialog_an_answer() {
        let mut answered = resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER));
        answered.to_tag = Some("fork-a".to_string());
        let mut silent = resp(3, BOB, ALICE, 200, 1, "INVITE", None);
        silent.to_tag = Some("fork-b".to_string());
        let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)), answered, silent];
        let f = violations(&Final2xxAnswersTheOffer, &msgs);
        assert_eq!(f.len(), 1, "one fork answered, the other did not: {f:?}");
        assert_eq!(f[0].anchor, 2);
    }

    // ---- second-answer-repeats-the-first --------------------------------

    /// The same media set as [`AUDIO_ANSWER`] at another address and port —
    /// the shape 11 of the corpus's 59 two-dialog reroutes differ by.
    const MOVED_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.3\r\n\
t=0 0\r\n\
m=audio 41000 RTP/AVP 8\r\n";

    /// A media set [`AUDIO_ANSWER`]'s is not — the shape the other 48 differ
    /// by.
    const WIDER_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n\
m=video 60790 RTP/AVP 96\r\n";

    /// A reliable provisional carrying `Require: 100rel` and an `RSeq` — its
    /// description binds (RFC 3262 §5).
    fn reliable(at_us: u64, status: u16, body: Option<&str>) -> Msg {
        Msg {
            head: Some(head("Require: 100rel\r\nRSeq: 1\r\n", body)),
            ..resp(at_us, BOB, ALICE, status, 1, "INVITE", body)
        }
    }

    /// §13.2.1's everyday form: the reliable provisional states the answer and
    /// the 2xx repeats it, which is what the RFC admits.
    #[test]
    fn a_second_answer_repeating_the_plan_is_clean() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            reliable(2, 183, Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
        assert_eq!(out.len(), 1, "the 2xx is the one occasion: {out:?}");
        assert_eq!((out[0].emitter.as_str(), out[0].cseq), (BOB, 1), "the answerer");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    /// Early media in an UNRELIABLE provisional states no answer: an
    /// announcement the peer may re-latch on before the callee answers is
    /// RFC 3960's shape, and the 2xx is the dialog's first binding answer.
    #[test]
    fn early_media_in_an_unreliable_provisional_binds_nothing() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 183, 1, "INVITE", Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", Some(MOVED_ANSWER)),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
        assert!(out.is_empty(), "the 2xx is the answer, not a second one: {out:?}");
    }

    #[test]
    fn a_second_answer_moving_the_media_is_flagged() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            reliable(2, 183, Some(AUDIO_ANSWER)),
            resp(3, BOB, ALICE, 200, 1, "INVITE", Some(MOVED_ANSWER)),
            req(4, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&SecondAnswerRepeatsTheFirst, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].anchor, f[0].emitter.as_str()), (2, BOB), "the 2xx, on its sender");
        let Decision::Violated(Evidence::SecondAnswerDiverged {
            offer_msg,
            first_answer_msg,
            first_plan,
            second_plan,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!((*offer_msg, *first_answer_msg), (0, 1));
        assert_eq!(
            first_plan,
            &vec!["c=IN IP4 10.0.0.2".to_string(), "audio 60788 RTP/AVP 8".to_string()]
        );
        assert_eq!(
            second_plan,
            &vec!["c=IN IP4 10.0.0.3".to_string(), "audio 41000 RTP/AVP 8".to_string()]
        );
    }

    /// The control the corpus supplies: two forks answering one INVITE carry
    /// two genuinely different answers under two To tags, and neither is a
    /// second answer to the other.
    #[test]
    fn two_forks_answer_one_offer_and_neither_is_a_second_answer() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp_fork(2, 200, 1, "b1", Some(AUDIO_ANSWER)),
            resp_fork(3, 200, 1, "b2", Some(WIDER_ANSWER)),
        ];
        let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
        assert!(out.is_empty(), "each fork closes its own dialog: {out:?}");
    }

    /// The same two answers folded onto ONE To tag — the collapsed fork the
    /// obligation exists for.
    #[test]
    fn a_collapsed_fork_answering_twice_on_one_dialog_is_flagged() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp_fork(2, 200, 1, "b1", Some(AUDIO_ANSWER)),
            resp_fork(3, 200, 1, "b1", Some(WIDER_ANSWER)),
        ];
        let f = violations(&SecondAnswerRepeatsTheFirst, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 2, "the second 2xx on the one dialog");
        let Decision::Violated(Evidence::SecondAnswerDiverged { second_plan, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(second_plan.len(), 3, "c= plus two streams: {second_plan:?}");
    }

    #[test]
    fn a_retransmitted_answer_is_not_a_second_answer() {
        let mut msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
        ];
        let mut again = msgs[1].clone();
        again.at_us = 3;
        again.repeat = true;
        msgs.push(again);
        let out = run(&SecondAnswerRepeatsTheFirst, &msgs);
        assert!(out.is_empty(), "a retransmission states no second answer: {out:?}");
    }

    /// A re-INVITE opens a round of its own, so its answer answers THAT offer
    /// and is no second answer to the first round's.
    #[test]
    fn a_re_invite_answer_is_no_second_answer() {
        let out = run(&SecondAnswerRepeatsTheFirst, &t38_call(None, None));
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- answer-stream-matches-offer -----------------------------------

    #[test]
    fn answer_keeping_media_type_and_proto_is_clean() {
        // Judged from the ANSWERER: it takes the offer and sends the answer.
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(T38_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let out = run(&AnswerStreamMatchesOffer, &msgs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
        assert_eq!(out[0].emitter, BOB, "the answerer is charged");
    }

    #[test]
    fn answer_re_typing_the_stream_is_flagged() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(T38_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&AnswerStreamMatchesOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::AnswerStreamRetyped {
            stream_indexes,
            offered,
            answered,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(stream_indexes, &vec![0]);
        assert_eq!(offered, &vec!["image 27500 udptl".to_string()]);
        assert_eq!(answered, &vec!["audio 60788 RTP/AVP".to_string()]);
    }

    #[test]
    fn answer_re_transporting_the_stream_is_flagged() {
        const SAVP_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/SAVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(SAVP_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&AnswerStreamMatchesOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::AnswerStreamRetyped { offered, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(offered, &vec!["audio 27500 RTP/AVP".to_string()]);
    }

    #[test]
    fn rejected_stream_keeps_type_and_proto_at_port_zero() {
        const REJECTED: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(REJECTED)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        assert!(violations(&AnswerStreamMatchesOffer, &msgs).is_empty());
    }

    #[test]
    fn every_re_typed_stream_of_one_answer_rides_one_finding() {
        const TWO_STREAM_OFFER: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
m=video 27502 RTP/AVP 96\r\n";
        const TWO_STREAM_ANSWER: &str = "v=0\r\n\
o=bob 2 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n\
m=audio 60790 RTP/SAVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(TWO_STREAM_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(TWO_STREAM_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ];
        let f = violations(&AnswerStreamMatchesOffer, &msgs);
        assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
        let Decision::Violated(Evidence::AnswerStreamRetyped { stream_indexes, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(stream_indexes, &vec![0, 1], "both streams named on the one finding");
    }

    #[test]
    fn a_second_round_is_judged_like_the_first() {
        const RETYPED_T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
            req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(T38_OFFER)),
            resp(5, BOB, ALICE, 200, 3, "INVITE", Some(RETYPED_T38_ANSWER)),
            req(6, ALICE, BOB, "ACK", 3, Some("bt"), None),
        ];
        let f = violations(&AnswerStreamMatchesOffer, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].cseq, 3, "the second round's offer transaction");
    }

    // ---- sdp-origin-continuity -----------------------------------------

    #[test]
    fn rising_session_version_on_one_origin_is_clean() {
        let msgs = t38_call(None, None);
        assert!(violations(&SdpOriginContinuity, &msgs).is_empty());
    }

    #[test]
    fn a_foreign_origin_mid_dialog_is_flagged() {
        let msgs = t38_call(None, Some(STRAY_ANSWER));
        let f = violations(&SdpOriginContinuity, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 6, "the ACK carrying the foreign origin");
        let Decision::Violated(Evidence::SdpOriginDiverged {
            same_session,
            origin_line,
            prior_origin_line,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert!(!same_session);
        assert_eq!(origin_line, "o=bob 1 1 IN IP4 127.0.0.1");
        assert_eq!(prior_origin_line, "o=alice 424242 2 IN IP4 10.0.0.1");
    }

    #[test]
    fn a_lowered_session_version_is_flagged() {
        const STALE_REOFFER: &str = "v=0\r\n\
o=alice 424242 0 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
            req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(STALE_REOFFER)),
        ];
        let f = violations(&SdpOriginContinuity, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::SdpOriginDiverged {
            same_session,
            session_version,
            prior_session_version,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert!(same_session, "same session, stale revision");
        assert_eq!((*prior_session_version, *session_version), (1, 0));
    }

    /// The address is part of the identity, so a re-offer from a new one is a
    /// different session even where username and sess-id repeat.
    #[test]
    fn a_moved_origin_address_is_a_different_session() {
        const MOVED: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 192.0.2.9\r\n\
s=-\r\n\
c=IN IP4 192.0.2.9\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
            req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(MOVED)),
        ];
        let f = violations(&SdpOriginContinuity, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::SdpOriginDiverged { same_session, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert!(!same_session);
    }

    /// The version tracks the changes: a changed description owes exactly +1
    /// and a byte-identical one owes the version it already had.
    #[test]
    fn the_version_tracks_what_the_description_says() {
        const BUMPED_TWICE: &str = "v=0\r\n\
o=alice 424242 3 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(BUMPED_TWICE)),
        ];
        let f = violations(&SdpOriginContinuity, &msgs);
        assert_eq!(f.len(), 1, "a changed body owes exactly +1: {f:?}");

        const BUMPED_FOR_NOTHING: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(BUMPED_FOR_NOTHING)),
        ];
        let f = violations(&SdpOriginContinuity, &msgs);
        assert_eq!(f.len(), 1, "a byte-identical description owes an unchanged version: {f:?}");
        let Decision::Violated(Evidence::SdpOriginDiverged { body_changed, .. }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert!(!body_changed);
    }

    #[test]
    fn a_taken_peer_origin_never_lands_on_the_taker() {
        // ALICE only TAKES bob's descriptions here — its own origin stream is
        // empty, so bob's discontinuity is bob's finding, on bob's slot.
        let msgs = vec![
            req(1, BOB, ALICE, "INVITE", 1, None, Some(AUDIO_OFFER)),
            resp(2, ALICE, BOB, 200, 1, "INVITE", Some(AUDIO_ANSWER)),
            req(3, BOB, ALICE, "ACK", 1, Some("bt"), Some(STRAY_ANSWER)),
        ];
        let f = violations(&SdpOriginContinuity, &msgs);
        assert!(f.iter().all(|f| f.emitter == BOB), "{f:?}");
    }

    #[test]
    fn a_description_with_no_readable_origin_settles_nothing() {
        const NO_ORIGIN: &str = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 27500 RTP/AVP 8\r\n";
        // Same origin as AUDIO_OFFER, changed description, version not moved.
        const STALE_AFTER_THE_GAP: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n\
a=sendonly\r\n";
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(AUDIO_OFFER)),
            req(2, ALICE, BOB, "INVITE", 3, Some("bt"), Some(NO_ORIGIN)),
            // The stream's predecessor is the last description that HAD an
            // origin, so the stale re-offer is still caught across the gap.
            req(3, ALICE, BOB, "INVITE", 5, Some("bt"), Some(STALE_AFTER_THE_GAP)),
        ];
        let out = run(&SdpOriginContinuity, &msgs);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out.iter().any(|f| f.anchor == 1 && !f.decided()));
        let hit = out.iter().find(|f| f.anchor == 2).expect("the re-stated origin");
        let Decision::Violated(Evidence::SdpOriginDiverged { prior_origin_msg, .. }) =
            &hit.decision
        else {
            panic!("{:?}", hit.decision);
        };
        assert_eq!(*prior_origin_msg, 0, "compared against the last readable origin");
    }

    // ---- the RFC 3264 offer/answer-model family -------------------------

    /// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
    const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    /// Its answer: same stream table, same `t=`, same payload-type bindings.
    const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    const OFFER_2MEDIA: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

    const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

    /// A response presenting a chosen early dialog — the To tag that tells two
    /// forks of one INVITE apart.
    fn resp_fork(at_us: u64, status: u16, cseq: u32, to_tag: &str, body: Option<&str>) -> Msg {
        Msg {
            to_tag: Some(to_tag.to_string()),
            ..resp(at_us, BOB, ALICE, status, cseq, "INVITE", body)
        }
    }

    /// INVITE(offer) → 200(answer) → ACK: one round, alice offering.
    fn one_round(offer: &str, answer: &str) -> Vec<Msg> {
        vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(offer)),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(answer)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), None),
        ]
    }

    // ---- no-new-offer-while-offer-pending ------------------------------

    #[test]
    fn a_serialised_offer_round_is_clean() {
        let out = run(&NoNewOfferWhileOfferPending, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "one occasion per offer sent: {out:?}");
        assert_eq!(out[0].emitter, ALICE, "the offerer is charged");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn a_second_offer_over_an_unanswered_one_is_flagged() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
            req(2, ALICE, BOB, "INVITE", 2, None, Some(OFFER_1AUDIO)),
        ];
        let f = violations(&NoNewOfferWhileOfferPending, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].anchor, f[0].emitter.as_str()), (1, ALICE));
        let Decision::Violated(Evidence::OfferWhilePending {
            pending_offer_msg,
            pending_offer_cseq,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!((*pending_offer_msg, *pending_offer_cseq), (0, 1));
    }

    /// §5 charges an agent for overtaking ITS OWN offer; a round it merely took
    /// is the peer's to answer and is the peer's occasion.
    #[test]
    fn an_offer_the_agent_only_took_never_blocks_its_own() {
        let msgs = vec![
            req(1, BOB, ALICE, "INVITE", 7, None, Some(AUDIO_OFFER)),
            req(2, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
        ];
        assert!(violations(&NoNewOfferWhileOfferPending, &msgs).is_empty());
    }

    #[test]
    fn a_delayed_offer_in_the_2xx_is_its_senders_own_offer() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, None),
            resp(2, BOB, ALICE, 200, 1, "INVITE", Some(AUDIO_OFFER)),
            req(3, ALICE, BOB, "ACK", 1, Some("bt"), Some(AUDIO_ANSWER)),
        ];
        let out = run(&NoNewOfferWhileOfferPending, &msgs);
        assert_eq!(out.len(), 1, "the 2xx offer is the one occasion: {out:?}");
        assert_eq!(out[0].emitter, BOB);
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    // ---- answer-m-line-count-matches-offer ------------------------------

    #[test]
    fn an_answer_with_the_offers_stream_count_is_clean() {
        let out = run(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].emitter, BOB, "the answerer is charged");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn an_answer_adding_a_stream_is_flagged() {
        let f = violations(&AnswerMLineCountMatchesOffer, &one_round(OFFER_1AUDIO, ANSWER_2MEDIA));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::AnswerMLineCountDiffers {
            offer_m_lines,
            answer_m_lines,
            answered_streams,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!((*offer_m_lines, *answer_m_lines), (1, 2));
        assert_eq!(
            answered_streams,
            &vec!["audio/RTP/AVP".to_string(), "video/RTP/AVP".to_string()]
        );
    }

    /// Two forks are two answers to ONE offer (the To tag partitions the
    /// round's closures), so each fork's answer is judged on its own dialog and
    /// a bad one behind a clean fork is not hidden by it.
    #[test]
    fn each_fork_answers_the_one_offer_on_its_own_dialog() {
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(OFFER_1AUDIO)),
            resp_fork(2, 180, 1, "b1", None),
            resp_fork(3, 200, 1, "b2", Some(ANSWER_2MEDIA)),
            resp_fork(4, 200, 1, "b1", Some(ANSWER_1AUDIO)),
        ];
        let out = run(&AnswerMLineCountMatchesOffer, &msgs);
        assert_eq!(out.len(), 2, "one occasion per fork's answer: {out:?}");
        let f: Vec<&Finding> = out.iter().filter(|f| f.violated()).collect();
        assert_eq!(f.len(), 1, "only the fork that answered wrongly: {f:?}");
        assert_eq!(f[0].anchor, 2, "the two-stream answer on fork b2");
    }

    // ---- answer-t-line-equals-offer -------------------------------------

    #[test]
    fn an_answer_repeating_the_t_line_is_clean() {
        let out = run(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn an_answer_rewriting_the_t_line_is_flagged() {
        let answer = ANSWER_1AUDIO.replace("t=0 0", "t=100 200");
        let f = violations(&AnswerTLineEqualsOffer, &one_round(OFFER_1AUDIO, &answer));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::AnswerTLineDiffers {
            offer_t_line, answer_t_line, ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!((offer_t_line.as_str(), answer_t_line.as_str()), ("0 0", "100 200"));
    }

    // ---- answer-media-type-matches-offer --------------------------------

    #[test]
    fn an_answer_keeping_the_media_type_at_each_index_is_clean() {
        let out = run(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, ANSWER_2MEDIA));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn every_index_one_answer_swapped_rides_one_finding() {
        let swapped = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=sendrecv\r\n\
m=audio 50002 RTP/AVP 0\r\n\
a=sendrecv\r\n";
        let f = violations(&AnswerMediaTypeMatchesOffer, &one_round(OFFER_2MEDIA, swapped));
        assert_eq!(f.len(), 1, "one answer is one occasion: {f:?}");
        let Decision::Violated(Evidence::AnswerMediaTypeMismatched {
            stream_indexes,
            offered_types,
            answered_types,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(stream_indexes, &vec![0, 1], "both positions named on the one finding");
        assert_eq!(offered_types, &vec!["audio".to_string(), "video".to_string()]);
        assert_eq!(answered_types, &vec!["video".to_string(), "audio".to_string()]);
    }

    // ---- direction-pair-valid -------------------------------------------

    #[test]
    fn a_sendrecv_pair_is_clean() {
        let out = run(&DirectionPairValid, &one_round(OFFER_1AUDIO, ANSWER_1AUDIO));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn an_answer_of_sendonly_to_sendonly_is_flagged() {
        let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=sendonly");
        let answer = ANSWER_1AUDIO.replace("a=sendrecv", "a=sendonly");
        let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::DirectionPairInvalid {
            offered_directions,
            answered_directions,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(offered_directions, &vec!["sendonly".to_string()]);
        assert_eq!(answered_directions, &vec!["sendonly".to_string()]);
    }

    /// An `m=` block with no direction attribute states `sendrecv` (§6.1), so
    /// an offer of `inactive` answered by a bare block is a real mis-pairing.
    #[test]
    fn a_bare_answer_block_reads_as_sendrecv() {
        let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
        let answer = ANSWER_1AUDIO.replace("a=sendrecv\r\n", "");
        let f = violations(&DirectionPairValid, &one_round(&offer, &answer));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::DirectionPairInvalid { answered_directions, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(answered_directions, &vec!["sendrecv".to_string()]);
    }

    // ---- rejected-stream-minimal-answer ---------------------------------

    #[test]
    fn a_rejection_keeping_a_format_token_is_clean() {
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0");
        let out = run(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn a_bare_rejection_is_flagged() {
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP");
        let f = violations(&RejectedStreamMinimalAnswer, &one_round(OFFER_1AUDIO, &answer));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::RejectedStreamWithoutFormat {
            stream_indexes,
            rejected_rows,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(stream_indexes, &vec![0]);
        assert_eq!(rejected_rows, &vec!["audio 0 RTP/AVP".to_string()]);
    }

    // ---- re-offer-m-line-count-monotonic --------------------------------

    #[test]
    fn a_re_offer_disabling_a_stream_in_place_is_clean() {
        let reoffer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
        msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
        let out = run(&ReOfferMLineCountMonotonic, &msgs);
        assert_eq!(out.len(), 1, "one occasion per step of alice's stream: {out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn a_re_offer_dropping_a_stream_slot_is_flagged() {
        let mut msgs = one_round(OFFER_2MEDIA, ANSWER_2MEDIA);
        msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(OFFER_1AUDIO)));
        let f = violations(&ReOfferMLineCountMonotonic, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].anchor, f[0].emitter.as_str()), (3, ALICE));
        let Decision::Violated(Evidence::ReOfferStreamsDropped {
            m_lines,
            prior_m_lines,
            prior_offer_msg,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!((*prior_m_lines, *m_lines, *prior_offer_msg), (2, 1, 0));
    }

    // ---- zero-port-propagation ------------------------------------------

    #[test]
    fn an_answer_keeping_a_disabled_stream_disabled_is_clean() {
        let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(out[0].decision, Decision::Compliant));
    }

    #[test]
    fn an_answer_reviving_a_disabled_stream_is_flagged() {
        let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let f = violations(&ZeroPortPropagation, &one_round(&offer, ANSWER_1AUDIO));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::ZeroPortResurrected {
            stream_indexes,
            answered_ports,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(
            (stream_indexes.as_slice(), answered_ports.as_slice()),
            ([0].as_slice(), [50000].as_slice())
        );
    }

    #[test]
    fn an_answer_stating_no_readable_port_settles_nothing() {
        let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio - RTP/AVP 0 96");
        let out = run(&ZeroPortPropagation, &one_round(&offer, &answer));
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(!out[0].decided(), "{out:?}");
    }

    // ---- payload-type-mapping-stable ------------------------------------

    #[test]
    fn a_re_offer_repeating_its_bindings_is_clean() {
        let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
        msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(OFFER_1AUDIO)));
        let out = run(&PayloadTypeMappingStable, &msgs);
        assert_eq!(out.len(), 3, "one occasion per description sent: {out:?}");
        assert!(out.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{out:?}");
    }

    #[test]
    fn a_re_offer_rebinding_a_payload_type_is_flagged() {
        let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
        let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
        msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
        let f = violations(&PayloadTypeMappingStable, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].anchor, f[0].emitter.as_str()), (3, ALICE));
        let Decision::Violated(Evidence::PayloadTypeRemapped {
            payload_types,
            prior_encodings,
            encodings,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(payload_types, &vec!["96".to_string()]);
        assert_eq!(prior_encodings, &vec!["opus/48000/2".to_string()]);
        assert_eq!(encodings, &vec!["H264/90000".to_string()]);
    }

    /// Encoding names are MIME subtypes: one binding, whatever the case.
    #[test]
    fn a_re_offer_re_spelling_the_encoding_is_clean() {
        let reoffer = OFFER_1AUDIO.replace("a=rtpmap:0 PCMU/8000", "a=rtpmap:0 pcmu/8000");
        let mut msgs = one_round(OFFER_1AUDIO, ANSWER_1AUDIO);
        msgs.push(req(4, ALICE, BOB, "INVITE", 3, Some("bt"), Some(&reoffer)));
        assert!(violations(&PayloadTypeMappingStable, &msgs).is_empty());
    }

    /// The bindings are the SENDER's: §3264 lets the two ends number the same
    /// codec differently, so the answerer's own numbering rebinds nothing.
    #[test]
    fn the_peers_own_numbering_is_not_the_agents_remap() {
        let answer = ANSWER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
        assert!(violations(&PayloadTypeMappingStable, &one_round(OFFER_1AUDIO, &answer)).is_empty());
    }

    /// A binding one `m=` block states binds the blocks below it too.
    #[test]
    fn a_description_rebinding_a_payload_type_within_itself_is_flagged() {
        let offer = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 96\r\n\
a=rtpmap:96 opus/48000/2\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n";
        let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(offer))];
        let f = violations(&PayloadTypeMappingStable, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::PayloadTypeRemapped { payload_types, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision);
        };
        assert_eq!(payload_types, &vec!["96".to_string()]);
    }

    /// An audio offer binding payload type 8 to `enc`.
    fn audio_pt8(enc: &str) -> String {
        format!(
            "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 8\r\n\
a=rtpmap:8 {enc}\r\n\
a=sendrecv\r\n"
        )
    }

    /// Two descriptions alice sent on one call, binding payload type 8 the way
    /// each names it.
    fn pt8_stream(first: &str, then: &str) -> Vec<Msg> {
        vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(&audio_pt8(first))),
            req(2, ALICE, BOB, "INVITE", 2, Some("bt"), Some(&audio_pt8(then))),
        ]
    }

    /// RFC 4566 §6 makes the channel count optional on an audio stream and its
    /// default one, so the two spellings state one binding — in either order.
    #[test]
    fn an_absent_audio_channel_count_is_one_channel() {
        for (first, then) in [("PCMA/8000", "PCMA/8000/1"), ("PCMA/8000/1", "PCMA/8000")] {
            let f = violations(&PayloadTypeMappingStable, &pt8_stream(first, then));
            assert!(f.is_empty(), "{first} then {then}: {f:?}");
        }
    }

    /// The default settles the channel count alone: a different encoding name,
    /// and a genuinely different channel count, are still re-binds.
    #[test]
    fn a_real_rebinding_of_an_audio_payload_type_still_fires() {
        for (first, then) in [("PCMA/8000", "PCMU/8000"), ("PCMA/8000", "PCMA/8000/2")] {
            let f = violations(&PayloadTypeMappingStable, &pt8_stream(first, then));
            assert_eq!(f.len(), 1, "{first} then {then}: {f:?}");
            let Decision::Violated(Evidence::PayloadTypeRemapped {
                payload_types,
                prior_encodings,
                encodings,
                ..
            }) = &f[0].decision
            else {
                panic!("{:?}", f[0].decision);
            };
            assert_eq!(payload_types, &vec!["8".to_string()]);
            assert_eq!(prior_encodings, &vec![first.to_string()], "the finding quotes the wire");
            assert_eq!(encodings, &vec![then.to_string()], "the finding quotes the wire");
        }
    }

    /// The encodings are compared as the block under judgement spells them, so
    /// one text repeated across media types stays one binding.
    #[test]
    fn one_encoding_text_across_media_types_is_one_binding() {
        let offer = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 99\r\n\
a=rtpmap:99 X-FOO/8000\r\n\
m=video 49172 RTP/AVP 99\r\n\
a=rtpmap:99 X-FOO/8000\r\n";
        let msgs = vec![req(1, ALICE, BOB, "INVITE", 1, None, Some(offer))];
        assert!(violations(&PayloadTypeMappingStable, &msgs).is_empty());
    }

    /// The default is audio's alone (RFC 4566 §6): outside audio the third
    /// subfield carries codec parameters, so no reading makes it a `1`.
    #[test]
    fn the_channel_count_default_is_not_read_outside_audio() {
        let video = |enc: &str| {
            format!(
                "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 {enc}\r\n"
            )
        };
        let msgs = vec![
            req(1, ALICE, BOB, "INVITE", 1, None, Some(&video("H264/90000"))),
            req(2, ALICE, BOB, "INVITE", 2, Some("bt"), Some(&video("H264/90000/1"))),
        ];
        assert_eq!(violations(&PayloadTypeMappingStable, &msgs).len(), 1);
    }

    // ---- the two per-description obligations ----------------------------

    /// An INVITE alice sent, its body declared under `content_type`.
    fn declared(body: &str, content_type: &str) -> Msg {
        let mut m = req(1, ALICE, BOB, "INVITE", 1, None, Some(body));
        m.head = Some(format!("Content-Type: {content_type}\r\n\r\n").into_bytes());
        m
    }

    const GOOD_SDP: &str = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=ptime:20\r\n";

    #[test]
    fn a_well_formed_description_is_compliant() {
        let f = run(&SdpBodyParseable, &[declared(GOOD_SDP, "application/sdp")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The `o=` fields are digit strings of any width: an NTP-magnitude
    /// `sess-version` is what RFC 4566 §5.2 recommends, not a defect.
    #[test]
    fn a_sixty_four_bit_session_version_is_compliant() {
        let body = "v=0\r\no=- 2170552860 2736569745311754808 IN IP4 203.0.113.17\r\ns=-\r\n\
                    c=IN IP4 203.0.113.17\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
        let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The grammar walk's own failures, each surfacing verbatim.
    #[test]
    fn every_grammar_failure_is_named_on_its_message() {
        let cases = [
            (
                "v=0\r\nv=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\n",
                "session descriptions",
            ),
            (
                "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=ptime:0\r\n",
                "ptime",
            ),
            (
                "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
                "no c= line",
            ),
        ];
        for (body, needle) in cases {
            let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
            let Decision::Violated(Evidence::SdpBodyRejected { sdp_failure, .. }) = &f[0].decision
            else {
                panic!("sdp evidence: {:?}", f[0].decision)
            };
            assert!(sdp_failure.contains(needle), "{sdp_failure} lacks {needle}");
        }
    }

    /// A body under another format is not this rule's to read, and neither is a
    /// message that carried none.
    #[test]
    fn an_undeclared_or_bodiless_message_is_no_occasion() {
        assert!(run(&SdpBodyParseable, &[declared("not sdp at all", "text/plain")]).is_empty());
        assert!(run(&SdpBodyParseable, &[declared("", "application/sdp")]).is_empty());
        let mut headless = declared(GOOD_SDP, "application/sdp");
        headless.head = None;
        assert!(run(&SdpBodyParseable, &[headless]).is_empty());
    }

    /// Held (`c=0.0.0.0`) AND rejected (port 0) at once, at session level and
    /// at media level.
    #[test]
    fn a_held_and_rejected_stream_is_violated() {
        let session = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                       m=audio 0 RTP/AVP 0\r\n";
        let media = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                     m=audio 0 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\n";
        for body in [session, media] {
            let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
            let Decision::Violated(Evidence::HeldAndRejectedStreams {
                held_stream_indexes,
                held_streams,
                held_c_lines,
                ..
            }) = &f[0].decision
            else {
                panic!("held/rejected evidence: {:?}", f[0].decision)
            };
            assert_eq!(held_stream_indexes.as_slice(), [0]);
            assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP"]);
            assert!(held_c_lines[0].contains("0.0.0.0"), "{:?}", held_c_lines);
        }
    }

    /// Held with a live port, rejected at a real address, and a block whose own
    /// `c=` overrides an unspecified session one — none is the ambiguity.
    #[test]
    fn one_disposition_at_a_time_is_compliant() {
        let bodies = [
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 49170 RTP/AVP 0\r\n",
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\n",
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\nc=IN IP4 10.0.0.1\r\n",
        ];
        for body in bodies {
            let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(matches!(f[0].decision, Decision::Compliant), "{body}: {:?}", f[0].decision);
        }
    }

    /// Two contradictory streams in one body are ONE finding naming both.
    #[test]
    fn every_offending_stream_rides_one_finding() {
        let body = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                    m=audio 0 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
        let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
        assert_eq!(f.len(), 1, "one description, one finding: {f:?}");
        let Decision::Violated(Evidence::HeldAndRejectedStreams {
            held_stream_indexes,
            held_streams,
            ..
        }) = &f[0].decision
        else {
            panic!("held/rejected evidence: {:?}", f[0].decision)
        };
        assert_eq!(held_stream_indexes.as_slice(), [0, 1]);
        assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP", "video 0 RTP/AVP"]);
    }

    /// A message carrying no description at all opens no occasion.
    #[test]
    fn a_bodiless_message_is_no_c0_occasion() {
        assert!(run(&C0PortNonZero, &[req(1, ALICE, BOB, "BYE", 2, Some("bt"), None)]).is_empty());
    }
}
