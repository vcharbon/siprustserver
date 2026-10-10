//! The rows the family's reading collects, and the per-agent state it keeps.

use std::collections::{BTreeMap, BTreeSet};

/// One agent's offer/answer state on one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct AgentKey<'a> {
    /// The endpoint whose state this is.
    pub(super) agent: &'a str,
    pub(super) call_id: &'a str,
}

/// The transaction an offer rode, as ONE agent sees it: the CSeq number, the
/// method, and whether THIS agent sent the request that carried it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RoundKey {
    pub(super) cseq: u32,
    /// The method, ASCII-uppercased — the wire may spell it either way.
    pub(super) method: String,
    pub(super) request_sent_by_agent: bool,
}

/// One offer/answer round as one agent drove it.
#[derive(Debug)]
pub(super) struct Round<'a> {
    /// Index into the view's `msgs` of the description that opened the round.
    pub(super) offer: usize,
    /// The CSeq number of the transaction that offer rode.
    pub(super) offer_cseq: u32,
    /// The agent SENT the description that opened this round, rather than
    /// taking it — the round is an offer of ITS OWN. Read by
    /// [`NoNewOfferWhileOfferPending`](super::NoNewOfferWhileOfferPending), which §5 charges only for overtaking
    /// the agent's own pending offer.
    pub(super) offer_sent: bool,
    /// The early dialog the offer's own message named (its To tag), empty on an
    /// INITIAL request — which every dialog its forks open answers. Two early
    /// dialogs number their requests independently (§12.2.1.1), so an in-dialog
    /// round belongs to the one dialog whose tag it carries.
    pub(super) offer_dialog: &'a str,
    /// Per early dialog (the To tag that tells two forks apart): the index into
    /// the view's `msgs` of the description that closed the round there.
    pub(super) answered: BTreeMap<&'a str, usize>,
    /// Per early dialog: the index into the view's `msgs` of the description
    /// that stated the BINDING answer there — a final, or a reliable
    /// provisional (§13.2.1, RFC 3262 §5). An unreliable provisional's
    /// description is early media the peer may re-latch on and binds nothing,
    /// so it never enters here. Read by [`SecondAnswerRepeatsTheFirst`](super::SecondAnswerRepeatsTheFirst).
    pub(super) bound: BTreeMap<&'a str, usize>,
}

/// What the vantage can say about the round an ACK acknowledges.
#[derive(Debug, Clone, Copy)]
pub(super) enum AckRound {
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
pub(super) struct AckSighting<'a> {
    /// Index into the view's `msgs`.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    pub(super) cseq: u32,
    pub(super) round: AckRound,
}

/// One ACK an agent sent on a delayed-offer round (§13.2.1: the 2xx carried
/// the offer), whether or not it carried the answer.
#[derive(Debug)]
pub(super) struct DelayedAck<'a> {
    /// Index into the view's `msgs` of the ACK.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    pub(super) cseq: u32,
    /// Index into the view's `msgs` of the 2xx that carried the offer.
    pub(super) offer: usize,
    /// The ACK carried a description: the answer.
    pub(super) answered: bool,
    /// The vantage carried the ACK's body bytes, so its absence is a fact.
    pub(super) body_known: bool,
}

/// One round an agent CLOSED with a description of its own.
#[derive(Debug)]
pub(super) struct Answered<'a> {
    /// Index into the view's `msgs` of the closing description.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    /// Index into the view's `msgs` of the description that opened the round.
    pub(super) offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    pub(super) offer_cseq: u32,
}

/// One non-failure FINAL an agent sent on a round it took an offer for — the
/// last message §13.2.1 admits that round's answer in.
#[derive(Debug)]
pub(super) struct AnswerDue<'a> {
    /// Index into the view's `msgs` of the final.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    pub(super) status: u16,
    /// Index into the view's `msgs` of the description that opened the round.
    pub(super) offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    pub(super) offer_cseq: u32,
    /// Index into the view's `msgs` of the BINDING answer already on this
    /// dialog when the final went out — `None` where the final is the round's
    /// last occasion to state one.
    pub(super) bound: Option<usize>,
}

/// One BINDING description an agent sent onto a dialog whose answer it had
/// ALREADY stated there — a second answer, which the peer ignores (§13.2.1).
#[derive(Debug)]
pub(super) struct ReAnswer<'a> {
    /// Index into the view's `msgs` of the second description.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    /// Index into the view's `msgs` of the description that opened the round.
    pub(super) offer: usize,
    /// The CSeq number of the transaction the OFFER rode.
    pub(super) offer_cseq: u32,
    /// Index into the view's `msgs` of the answer that closed the round on this
    /// dialog — the one the peer is acting on.
    pub(super) first: usize,
}

/// One description an agent SENT that OPENED a round of its own, with the state
/// of its own rounds AT THAT MOMENT — not at the end of the walk.
#[derive(Debug)]
pub(super) struct OfferStep<'a> {
    /// Index into the view's `msgs`.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    pub(super) cseq: u32,
    /// Index into the view's `msgs` of the agent's own most recent offer that
    /// had drawn no answer yet when this one went out — `None` where none was
    /// outstanding.
    pub(super) pending: Option<usize>,
    /// The CSeq number of that pending offer's transaction.
    pub(super) pending_cseq: u32,
}

/// One description an agent SENT, against the one it sent before on that call.
#[derive(Debug)]
pub(super) struct OriginStep<'a> {
    /// Index into the view's `msgs`.
    pub(super) msg: usize,
    pub(super) src: &'a str,
    pub(super) dst: &'a str,
    pub(super) cseq: u32,
    /// Index into the view's `msgs` of the agent's previous description with a
    /// readable `o=` line on this call — its origin stream's predecessor.
    pub(super) prior: usize,
}

/// One agent's running state through the walk.
#[derive(Default)]
pub(super) struct AgentState<'a> {
    pub(super) rounds: Vec<Round<'a>>,
    /// Round index by the transaction that carried the offer.
    pub(super) round_at: BTreeMap<RoundKey, usize>,
    /// Round index of a DELAYED offer, keyed by the INVITE CSeq, whether this
    /// agent sent that INVITE, and the early dialog the 2xx presented.
    pub(super) delayed_at: BTreeMap<(u32, bool, &'a str), usize>,
    /// The same keys where a reliable provisional carried the offer first (RFC
    /// 3262 §5): its round closed by PRACK, so the 2xx's description is no
    /// offer the ACK owes an answer to.
    pub(super) offered_reliably: BTreeSet<(u32, bool, &'a str)>,
    /// Index into the view's `msgs` of this agent's last SENT description with
    /// a readable `o=` line on this call.
    pub(super) last_origin: Option<usize>,
    /// Indexes into the view's `msgs` of EVERY description this agent sent on
    /// this call, ascending — the stream [`Reading::sent_streams`](super::reading::Reading::sent_streams) publishes.
    pub(super) sent_docs: Vec<usize>,
}
