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

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sdp_doc::{self, SdpDirection, SdpDoc, SdpOrigin};
use sip_message::{sdp, sniff};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

crate::registry::rules! {
    /// RFC 3261 §13.2.2.4: an ACK closing a completed offer/answer exchange carries no body — the
    /// round has both halves and the ACK has no answer left to deliver. Charges the ACK's sender.
    AckBodyAfterCompleteOfferAnswer = "ack-body-after-complete-offer-answer", relayable;
    /// RFC 3261 §13.2.1 / RFC 3264 §6: the non-failure final to a request that carried an offer
    /// carries the answer, unless a reliable provisional already stated one on that dialog. Charges
    /// the answerer.
    Final2xxAnswersTheOffer = "final-2xx-answers-the-offer", relayable;
    /// RFC 3261 §13.2.2.4 / §13.2.1: an ACK to a 2xx that carried a delayed offer carries the
    /// answer. Charges the ACK's sender.
    DelayedOfferAnsweredInAck = "delayed-offer-answered-in-ack";
    /// RFC 3261 §13.2.1: one dialog carries ONE answer — a later description on that dialog
    /// re-states the transport plan the first stated, never another one. Charges the answerer.
    SecondAnswerRepeatsTheFirst = "second-answer-repeats-the-first";
    /// RFC 3264 §6: the `m=` line an answer sends back for an offered stream re-states that
    /// stream's media type and transport — rejecting it means port 0, never re-typing it. Charges
    /// the answerer.
    AnswerStreamMatchesOffer = "answer-stream-matches-offer";
    /// RFC 4566 §5.2 / RFC 3264 §8: every session description an agent sends on one call keeps the
    /// `o=` identity and moves `sess-version` by exactly what changed. Charges the description's
    /// sender.
    SdpOriginContinuity = "sdp-origin-continuity";
    /// RFC 3264 §5: an agent holds one offer of its own outstanding at a time on a call — the next
    /// waits for the answer to the last. Charges the offer's sender.
    NoNewOfferWhileOfferPending = "no-new-offer-while-offer-pending";
    /// RFC 3264 §6: an answer holds exactly one `m=` line per offered stream — a rejected stream
    /// keeps its slot at port 0, it never disappears. Charges the answerer.
    AnswerMLineCountMatchesOffer = "answer-m-line-count-matches-offer";
    /// RFC 3264 §6: an answer repeats the offer's `t=` line — the two ends state one session time.
    /// Charges the answerer.
    AnswerTLineEqualsOffer = "answer-t-line-equals-offer";
    /// RFC 3264 §6.1: the answer's `m=` line at each offered position states that position's media
    /// type — the per-stream pairing both ends index on. Charges the answerer.
    AnswerMediaTypeMatchesOffer = "answer-media-type-matches-offer";
    /// RFC 3264 §6.1: an answer's direction attribute is one the offer's admits — `sendonly` draws
    /// `recvonly`/`inactive`, `inactive` draws `inactive`. Charges the answerer.
    DirectionPairValid = "direction-pair-valid";
    /// RFC 3264 §6: an answer rejecting a stream still lists a media format on its `m=` line — a
    /// bare `m=audio 0 RTP/AVP` is no description at all. Charges the answerer.
    RejectedStreamMinimalAnswer = "rejected-stream-minimal-answer";
    /// RFC 3264 §8: an agent's later description on a call carries at least as many `m=` lines as
    /// the one before — a removed stream keeps its slot at port 0. Charges the description's
    /// sender.
    ReOfferMLineCountMonotonic = "re-offer-m-line-count-monotonic";
    /// RFC 3264 §8: a stream offered at port 0 is answered at port 0 — a disabled stream stays
    /// disabled. Charges the answerer.
    ZeroPortPropagation = "zero-port-propagation";
    /// RFC 3264 §8.3.2: once an agent's description binds a payload type to an encoding, its later
    /// descriptions on that call keep the binding — the peer caches it. Charges the description's
    /// sender.
    PayloadTypeMappingStable = "payload-type-mapping-stable", relayable;
    /// RFC 3264 §5-6 / RFC 4566 §5: a body sent as `application/sdp` satisfies the offer/answer
    /// grammar. Charges the sender that minted it.
    SdpBodyParseable = "sdp-body-parseable";
    /// RFC 3264 §6 / §8.4: a stream held at `c=0.0.0.0` is not ALSO rejected at port 0 — the two
    /// together state no disposition a peer can act on. Charges the sender.
    C0PortNonZero = "c0-port-non-zero";
}

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

/// One ACK an agent sent on a delayed-offer round (§13.2.1: the 2xx carried
/// the offer), whether or not it carried the answer.
#[derive(Debug)]
struct DelayedAck<'a> {
    /// Index into the view's `msgs` of the ACK.
    msg: usize,
    src: &'a str,
    dst: &'a str,
    cseq: u32,
    /// Index into the view's `msgs` of the 2xx that carried the offer.
    offer: usize,
    /// The ACK carried a description: the answer.
    answered: bool,
    /// The vantage carried the ACK's body bytes, so its absence is a fact.
    body_known: bool,
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
    /// The same keys where a reliable provisional carried the offer first (RFC
    /// 3262 §5): its round closed by PRACK, so the 2xx's description is no
    /// offer the ACK owes an answer to.
    offered_reliably: BTreeSet<(u32, bool, &'a str)>,
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
    /// Every ACK an agent sent acknowledging a delayed-offer 2xx — the round's
    /// answer is owed in it — with the offer it owed. Read by
    /// [`DelayedOfferAnsweredInAck`].
    delayed_acks: Vec<DelayedAck<'a>>,
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
                relayed: false, // a relay lane is the consumer's to skip
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
/// another origin is [`SdpOriginContinuity`]'s occasion, and a second
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
                relayed: false, // a relay lane is the consumer's to skip
                anchor: r.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

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

/// **RFC 4566 §5.2 / RFC 3264 §8 — every session description an agent sends on
/// one call describes the SAME session, and its `sess-version` counts the
/// revisions.**
/// `o=<username> <sess-id> <sess-version> <nettype> <addrtype> <address>`
/// identifies it: a later description repeats all five identity
/// fields and raises `sess-version` by exactly one when the rest of the
/// description changed. A byte-identical description may keep its version or
/// raise it by one: §8 only has an unchanged version promise an identical
/// description. A changed identity makes the description a different session
/// the peer must treat as unrelated; a version that does not track the changes
/// leaves the peer unable to tell a re-offer from a repeat.
///
/// Charges the description's SENDER, against its own previous description on
/// that call. A description whose `o=` line no reader accepts settles nothing —
/// the stream's predecessor is the last one that did.
pub struct SdpOriginContinuity;

impl Obligation for SdpOriginContinuity {
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
            let ok = same_session && (delta == 1 || (delta == 0 && !body_changed));
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
                relayed: false, // a relay lane is the consumer's to skip
                anchor: s.msg,
                decision,
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3264 §6 — an answer holds exactly one `m=` line per offered stream.**
/// The two ends index streams by position, so a rejected stream keeps its slot
/// at port 0 and an added one is a re-offer's business, never an answer's. A
/// count that differs desyncs the stream table: from that position on, the two
/// ends mean different streams by the same index.
///
/// Charges the ANSWERER, on every round it closes and not only the first.
pub struct AnswerMLineCountMatchesOffer;

impl Obligation for AnswerMLineCountMatchesOffer {
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

/// **RFC 3264 §6 — an answer repeats the offer's `t=` line.** The session's
/// time bounds are the offerer's statement and the answer carries them back
/// verbatim; a rewritten `t=` leaves the two ends disagreeing on when the
/// session is active. A description carrying no `t=` line differs from one that
/// carries any.
///
/// Charges the ANSWERER, on every round it closes.
pub struct AnswerTLineEqualsOffer;

impl Obligation for AnswerTLineEqualsOffer {
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

#[cfg(test)]
mod tests;
