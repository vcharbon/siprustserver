//! The CSeq family of RFC 3261 — THREE obligations on one wire walk, each read
//! against the endpoint that TOOK the stream:
//!
//!   - [`CseqInDialogOrder`] (§12.2.1.1) charges the UAC: within a dialog it
//!     increments the CSeq by exactly one per new request, so the numbers it
//!     emits form a contiguous run with none used twice and none skipped.
//!   - [`ResponseCseqMatchesTransaction`] (§8.1.3.5 / §17) charges the UAS: a
//!     response copies its request's CSeq verbatim, so its `(number, method)`
//!     is one the requests on that top-Via branch actually carried.
//!   - [`AckCseqMatchesInvite`] (§13.2.2.4) charges the UAC: an ACK reuses the
//!     CSeq number of the INVITE it acknowledges, so an ACK's number is one an
//!     INVITE on its stream used.
//!
//! **A stream is one `(taker, Call-ID, From-tag)`** — one originating UA's
//! request flow in one direction, as one endpoint received it. Within a stream
//! each **dialog** is tracked by its To tag (`""` for the dialog-creating
//! request that has none yet): forking materialises several dialogs that share
//! Call-ID + From-tag but diverge by To tag, and each owns an independent CSeq
//! space (§12.1.2 / §13.2.2.4). Two forks' first PRACKs both at
//! `INVITE_CSeq + 1` is CORRECT, not a reuse; conflating them by ignoring the
//! To tag would be a false positive.
//!
//! **§12.2.1.1 is judged over the SET, not over arrival order.** It constrains
//! what the UAC *generates*, never the order a recording *observes*. Over a
//! lossy fabric a request can be dropped and recovered by re-emission (Timer
//! A/E/G); its recovered copy legitimately lands after a request the UAC sent
//! later (a REFER-progress NOTIFY at CSeq 3 recovered after the BYE at CSeq 4).
//! The UAC still incremented by exactly one and left no hole — the set is
//! `{…,3,4}`, contiguous — so the numbers are accumulated as they arrive and
//! contiguity + reuse are judged ONCE over the whole set. Only two things
//! flag: a **reuse** (one CSeq carried by two distinct new transactions on one
//! dialog) and a **gap** (a value the UAC skipped, leaving the sorted set
//! non-contiguous).
//!
//! **The retransmit fold keys on `(top-Via branch, method, CSeq)`, and the
//! branch alone is not enough.** A genuine on-wire retransmission repeats all
//! three. Method and CSeq are in the key because a simulated fabric's
//! per-worker `IdGen` resets on a failover restart, so a *different* request
//! relayed by the backup can reuse a branch the crashed primary spent — keying
//! on the branch alone would mis-skip it as a phantom retransmit (RFC 3261
//! §8.1.1.7 assumes globally unique branches; the fold does not). The fold is
//! this rule's OWN and does not consult [`Msg::repeat`]: the §17.2 mark is
//! keyed on `(Call-ID, branch, method)` plus byte identity, which the fold
//! already subsumes, and it cannot disambiguate that cross-failover branch
//! reuse.
//!
//! **A forked or confirmed dialog anchors on the attempt that ESTABLISHED it**
//! — the largest dialog-creating CSeq at or below its first in-dialog request,
//! folded in as the set's lower anchor. For a plain single-attempt call that is
//! the sole INVITE; under a deferred-auth §22.2 retry the empty-To-tag bucket
//! holds BOTH attempts (the challenged one, whose 401 established no dialog,
//! and the resent one), and anchoring on the abandoned lower attempt would
//! fabricate a gap. A first in-dialog request at or BELOW its anchor did not
//! advance at all: it reuses the INVITE's own number — a cross-dialog reuse the
//! per-To-tag bucketing otherwise hides — or regressed below it (CSeq 0
//! always).
//!
//! **ACK and CANCEL are exempt from §12.2.1.1** — they legitimately reuse the
//! CSeq of the request they acknowledge or cancel, and they do not advance the
//! dialog's sequence. That is why [`AckCseqMatchesInvite`] exists: the exemption
//! cannot catch an ACK that reuses the WRONG number, which is what a B2BUA
//! building its 2xx ACK from a `local_cseq` an early PRACK/UPDATE advanced
//! emits.
//!
//! **§8.1.1.5 (CSeq < 2^31) is deliberately NOT a rule here:** the parser's
//! registry-driven numeric pass (ADR-0007) rejects such a CSeq at ingest, so no
//! observed message can carry one.

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

/// **§12.2.1.1 — in-dialog request sequencing.** See the module doc.
///
/// The occasion is ONE new in-dialog request as the taker accounted it (after
/// the retransmit fold and the ACK/CANCEL exemption): it either continued the
/// dialog's run or it did not. Charges the endpoint that SENT it.
///
/// This is the teeth for a keepalive loop that never increments the dialog CSeq
/// (each new OPTIONS, and the eventual BYE, reuses the previous request's
/// number) and for a takeover that re-originates a dialog request from a stale
/// pre-failover CSeq snapshot (the survivor mints a `local_cseq + 1` the dialog
/// already spent). Both are invisible to a test UA that answers whatever it is
/// handed.
pub struct CseqInDialogOrder;

impl Obligation for CseqInDialogOrder {
    fn id(&self) -> RuleId {
        RuleId::CseqInDialogOrder
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, stream) in &seen.streams {
            // Every dialog-CREATING request (empty To tag) on this stream: one
            // INVITE for a plain call, both attempts under an auth retry.
            let creating: Vec<u32> = stream
                .dialogs
                .get("")
                .map(|d| d.by_cseq.keys().copied().collect())
                .unwrap_or_default();
            for (to_tag, dlg) in &stream.dialogs {
                let mut charged = dlg.charges(to_tag, &creating);
                for req in dlg.by_cseq.values().chain(dlg.reuses.iter()) {
                    out.push(Finding {
                        rule: RuleId::CseqInDialogOrder,
                        emitter: req.emitter.to_string(),
                        taker: key.taker.to_string(),
                        cseq: req.cseq,
                        relayed: false,
                        anchor: req.msg,
                        decision: match charged.remove(&req.msg) {
                            Some(evidence) => Decision::Violated(evidence),
                            None => Decision::Compliant,
                        },
                    });
                }
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **§8.1.3.5 / §17 — a response's CSeq is its request's.** A response is
/// matched to its client transaction by the topmost Via branch, and §8.1.3.5
/// has it copy the request's `CSeq` — sequence number *and* method — verbatim.
///
/// The occasion is ONE response the taker took. Charges the endpoint that sent
/// it. A branch whose requests this vantage never carried settles nothing, so
/// such a response is `Undecidable` rather than guessed at — only a positive
/// mismatch is a violation.
///
/// This is the teeth for a B2BUA that, failing to correlate an in-dialog
/// response to its pending request (a forked early dialog whose PRACK/UPDATE
/// 200 was looked up on the wrong fork), regenerates it on the *INVITE* server
/// transaction — a spurious `200 (INVITE)` carrying the PRACK's CSeq number on
/// the INVITE's branch. A real UAC discards it; the test UA accepts whatever
/// 200 it sees.
pub struct ResponseCseqMatchesTransaction;

impl Obligation for ResponseCseqMatchesTransaction {
    fn id(&self) -> RuleId {
        RuleId::ResponseCseqMatchesTransaction
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for rsp in &seen.responses {
            let head = |decision| Finding {
                rule: RuleId::ResponseCseqMatchesTransaction,
                emitter: rsp.emitter.to_string(),
                taker: rsp.taker.to_string(),
                cseq: rsp.cseq,
                relayed: false,
                anchor: rsp.msg,
                decision,
            };
            let Some(branch) = rsp.branch else {
                out.push(head(Decision::Undecidable("no via branch at this vantage")));
                continue;
            };
            let Some(carried) = seen.txn_cseqs.get(branch) else {
                out.push(head(Decision::Undecidable(
                    "no request on this transaction at this vantage",
                )));
                continue;
            };
            if carried.contains(&(rsp.cseq, rsp.method.to_ascii_uppercase())) {
                out.push(head(Decision::Compliant));
                continue;
            }
            out.push(head(Decision::Violated(Evidence::ResponseCseqUnmatched {
                mismatch_msg: rsp.msg,
                mismatch_hop: rsp.hop,
                mismatch_ts_us: rsp.ts_us,
                status: rsp.status,
                response_cseq: rsp.cseq,
                response_method: rsp.method.to_string(),
                txn_cseqs: carried.iter().map(|(n, m)| format!("{n} {m}")).collect(),
                branch: branch.to_string(),
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **§13.2.2.4 — the ACK for a 2xx reuses the INVITE's CSeq.** The ACK for a
/// 2xx (and the hop-by-hop ACK for a non-2xx final) carries the same CSeq
/// sequence number as the INVITE it acknowledges, so within a request stream
/// every ACK's number is one an INVITE on that stream used.
///
/// The occasion is ONE ACK the taker took, judged against the INVITEs the
/// stream had carried BY THEN — an ACK always follows its INVITE on the wire,
/// and a stream with no INVITE yet settles nothing (`Undecidable`). Charges the
/// endpoint that sent the ACK.
pub struct AckCseqMatchesInvite;

impl Obligation for AckCseqMatchesInvite {
    fn id(&self) -> RuleId {
        RuleId::AckCseqMatchesInvite
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, stream) in &seen.streams {
            for ack in &stream.acks {
                let head = |decision| Finding {
                    rule: RuleId::AckCseqMatchesInvite,
                    emitter: ack.emitter.to_string(),
                    taker: key.taker.to_string(),
                    cseq: ack.cseq,
                    relayed: false,
                    anchor: ack.msg,
                    decision,
                };
                let Some(known) = &ack.known else {
                    out.push(head(Decision::Undecidable(
                        "no INVITE on this stream at this vantage",
                    )));
                    continue;
                };
                if known.contains(&ack.cseq) {
                    out.push(head(Decision::Compliant));
                    continue;
                }
                out.push(head(Decision::Violated(Evidence::AckCseqUnmatched {
                    ack_msg: ack.msg,
                    ack_hop: ack.hop,
                    ack_ts_us: ack.ts_us,
                    ack_cseq: ack.cseq,
                    invite_cseqs: known.clone(),
                    from_tag: key.from_tag.to_string(),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One request stream as ONE endpoint TOOK it. Spelled out as a struct so the
/// three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct StreamKey<'a> {
    /// The endpoint the requests were addressed to — the vantage a consumer
    /// reads the far UAC's conduct from.
    taker: &'a str,
    call_id: &'a str,
    /// The From tag: one originating UA's flow in ONE direction, so the two
    /// directions of a dialog never share a CSeq space.
    from_tag: &'a str,
}

/// What one view's messages say about the CSeq obligations on it — the walk all
/// three rules read.
#[derive(Debug, Default)]
struct Reading<'a> {
    streams: BTreeMap<StreamKey<'a>, Stream<'a>>,
    /// Top-Via branch → the `(CSeq number, CSeq method uppercased)` pairs the
    /// REQUESTS on that transaction carried. Both directions of the view feed
    /// it: the transaction a response answers is one this vantage either opened
    /// or took.
    txn_cseqs: BTreeMap<&'a str, BTreeSet<(u32, String)>>,
    /// Every response the view carried, in observation order.
    responses: Vec<Response<'a>>,
}

/// One stream's accounting.
#[derive(Debug, Default)]
struct Stream<'a> {
    /// Transactions already accounted, keyed by `(top-Via branch, method, CSeq)`
    /// — the fold that turns an on-wire retransmission into one transaction.
    /// See the module doc for why the branch alone is not the key.
    seen_txns: BTreeSet<(&'a str, &'a str, u32)>,
    /// Per dialog (To tag, `""` = dialog-creating) → its accumulated CSeq set.
    dialogs: BTreeMap<&'a str, Dialog<'a>>,
    /// The INVITE CSeq numbers this stream has carried so far.
    invite_cseqs: BTreeSet<u32>,
    /// Every ACK the stream carried, each with the INVITE numbers known when it
    /// arrived.
    acks: Vec<Ack<'a>>,
}

/// One dialog's worth of new-request CSeq accounting, gathered
/// order-independently (see the module doc).
#[derive(Debug, Default)]
struct Dialog<'a> {
    /// Distinct new-transaction CSeq → the FIRST new transaction that carried
    /// it. A `BTreeMap`, so the keys come out ascending for the contiguity
    /// scan.
    by_cseq: BTreeMap<u32, Request<'a>>,
    /// The later new transactions that carried a CSeq already spent, in arrival
    /// order — each one a dialog CSeq that failed to advance.
    reuses: Vec<Request<'a>>,
}

impl<'a> Dialog<'a> {
    /// The evidence against each offending request of this dialog, keyed by its
    /// message index: a reuse, a skip, or a first request that never advanced
    /// past the dialog-creating anchor. Every other accounted request is
    /// compliant.
    fn charges(&self, to_tag: &str, creating: &[u32]) -> BTreeMap<usize, Evidence> {
        let mut charged = BTreeMap::new();
        for req in &self.reuses {
            let spent = &self.by_cseq[&req.cseq];
            charged.insert(
                req.msg,
                Evidence::CseqReused {
                    reuse_msg: req.msg,
                    reuse_hop: req.hop,
                    reuse_ts_us: req.ts_us,
                    spent_msg: spent.msg,
                    cseq: req.cseq,
                    method: req.method.to_string(),
                    reuse_branch: req.branch.to_string(),
                    spent_branch: spent.branch.to_string(),
                    from_tag: req.from_tag.to_string(),
                    to_tag: to_tag.to_string(),
                },
            );
        }
        let mut seqs: Vec<u32> = self.by_cseq.keys().copied().collect();
        // A dialog-creating request has no anchor of its own: its bucket IS the
        // baseline every tagged dialog measures from.
        if !to_tag.is_empty() {
            if let Some(&first) = seqs.first() {
                // The attempt this dialog advanced FROM: the largest
                // dialog-creating CSeq at or below its first in-dialog request.
                // Below every recorded attempt, fall back to the smallest so a
                // regression is measured against a real anchor rather than
                // escaping unjudged.
                let anchor = creating
                    .iter()
                    .copied()
                    .filter(|&c| c <= first)
                    .max()
                    .or_else(|| creating.iter().copied().min());
                if let Some(anchor) = anchor {
                    if first <= anchor {
                        let req = &self.by_cseq[&first];
                        charged.insert(req.msg, req.not_contiguous(anchor, to_tag));
                    } else if let Err(pos) = seqs.binary_search(&anchor) {
                        seqs.insert(pos, anchor);
                    }
                }
            }
        }
        for w in seqs.windows(2) {
            let (lo, hi) = (w[0], w[1]);
            // The offending message is the one that SKIPPED ahead (at `hi`); a
            // folded-in anchor sorts below every in-dialog number and so is
            // never `hi`.
            if hi > lo + 1 {
                if let Some(req) = self.by_cseq.get(&hi) {
                    charged.insert(req.msg, req.not_contiguous(lo, to_tag));
                }
            }
        }
        charged
    }
}

/// One new in-dialog request as the taker accounted it.
#[derive(Debug)]
struct Request<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    /// The request-line method, as the wire spelled it.
    method: &'a str,
    /// The top-Via branch, `""` where the vantage carried none.
    branch: &'a str,
    emitter: &'a str,
    from_tag: &'a str,
}

impl Request<'_> {
    fn not_contiguous(&self, prior_cseq: u32, to_tag: &str) -> Evidence {
        Evidence::CseqNotContiguous {
            skip_msg: self.msg,
            skip_hop: self.hop,
            skip_ts_us: self.ts_us,
            cseq: self.cseq,
            prior_cseq,
            method: self.method.to_string(),
            from_tag: self.from_tag.to_string(),
            to_tag: to_tag.to_string(),
        }
    }
}

/// One ACK the stream carried, with the INVITE numbers known when it arrived.
#[derive(Debug)]
struct Ack<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    emitter: &'a str,
    /// `None` where the stream had carried no INVITE yet — nothing to judge
    /// the ACK against.
    known: Option<Vec<u32>>,
}

/// One response the view carried.
#[derive(Debug)]
struct Response<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    cseq: u32,
    /// The CSeq header's method, as the wire spelled it.
    method: &'a str,
    branch: Option<&'a str>,
    emitter: &'a str,
    taker: &'a str,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// Absorb one message. A repeat is not skipped here: the §12.2.1.1 fold is
    /// the rule's own and strictly stronger (see the module doc), and the two
    /// transaction rules judge every copy the taker took, as a UAC does.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
        let method = match &msg.kind {
            Kind::Response { status } => {
                self.responses.push(Response {
                    msg: mi,
                    hop: msg.hop,
                    ts_us: msg.at_us,
                    status: *status,
                    cseq: msg.cseq,
                    method: msg.cseq_method.as_str(),
                    branch,
                    emitter: msg.src.as_str(),
                    taker: msg.dst.as_str(),
                });
                return;
            }
            Kind::Request { method } => method.as_str(),
        };
        // The transaction index reads the CSeq HEADER, which is what §8.1.3.5
        // has a response copy — and it needs no dialog identity, so it is filled
        // before the stream keying below can turn a message away.
        if let Some(branch) = branch {
            self.txn_cseqs
                .entry(branch)
                .or_default()
                .insert((msg.cseq, msg.cseq_method.to_ascii_uppercase()));
        }
        // A request with no From tag names no stream: it can key nothing.
        let Some(from_tag) = msg.from_tag.as_deref() else { return };
        let stream = self
            .streams
            .entry(StreamKey { taker: msg.dst.as_str(), call_id: msg.call_id.as_str(), from_tag })
            .or_default();

        // §13.2.2.4 accounting runs on EVERY copy, before the §12.2.1.1 fold:
        // an ACK that names no INVITE is wrong however often it is repeated.
        if msg.is_request("INVITE") {
            stream.invite_cseqs.insert(msg.cseq);
        } else if msg.is_request("ACK") {
            stream.acks.push(Ack {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                cseq: msg.cseq,
                emitter: msg.src.as_str(),
                known: (!stream.invite_cseqs.is_empty())
                    .then(|| stream.invite_cseqs.iter().copied().collect()),
            });
        }

        // A repeat of the SAME (branch, method, CSeq) is a retransmission of a
        // transaction already accounted: it opens no §12.2.1.1 occasion. A
        // request the vantage carried no branch for cannot be folded at all.
        if let Some(branch) = branch {
            if !stream.seen_txns.insert((branch, method, msg.cseq)) {
                return;
            }
        }
        // ACK and CANCEL legitimately reuse the related request's CSeq: exempt,
        // and they do not advance the per-dialog sequence.
        if msg.is_request("ACK") || msg.is_request("CANCEL") {
            return;
        }
        let req = Request {
            msg: mi,
            hop: msg.hop,
            ts_us: msg.at_us,
            cseq: msg.cseq,
            method,
            branch: branch.unwrap_or_default(),
            emitter: msg.src.as_str(),
            from_tag,
        };
        let dialog = stream.dialogs.entry(msg.to_tag.as_deref().unwrap_or_default()).or_default();
        match dialog.by_cseq.get(&req.cseq) {
            None => {
                dialog.by_cseq.insert(req.cseq, req);
            }
            // A DIFFERENT transaction carried a CSeq the dialog already spent →
            // the dialog CSeq failed to increment. Same branch, different method
            // is one transaction's shape restated, and records nothing new.
            Some(first) if first.branch != req.branch => dialog.reuses.push(req),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! The rules' OWN semantics under a CLOSED observation — what the adapters'
    //! vantage policy cannot state: which requests form one dialog, what the
    //! set alone settles about them, and where the wire settles nothing.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{AckCseqMatchesInvite, CseqInDialogOrder, ResponseCseqMatchesTransaction};

    const ALICE: &str = "10.0.0.1:5060";
    const BOB: &str = "10.0.0.2:5070";

    /// A request alice sent bob: method, CSeq, top-Via branch and dialog To tag
    /// all caller-controlled, so a test can model a new transaction (fresh
    /// branch), a retransmission (reused branch), a fork (distinct To tag) or a
    /// dialog-creating request (`None`).
    fn req(at_us: u64, method: &str, cseq: u32, branch: &str, to_tag: Option<&str>) -> Msg {
        Msg {
            at_us,
            src: ALICE.to_string(),
            dst: BOB.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: method.to_string(),
            from_tag: Some("fa".to_string()),
            to_tag: to_tag.map(str::to_string),
            via_branch: Some(branch.to_string()),
            head: None,
            body: None,
        }
    }

    /// A confirmed-dialog request (the `btag` dialog every plain-call test uses).
    fn in_dialog(at_us: u64, method: &str, cseq: u32, branch: &str) -> Msg {
        req(at_us, method, cseq, branch, Some("btag"))
    }

    /// A new-transaction OPTIONS keepalive (branch derived from the CSeq).
    fn options(at_us: u64, cseq: u32) -> Msg {
        in_dialog(at_us, "OPTIONS", cseq, &format!("z9hG4bK-{cseq}"))
    }

    /// A response bob sent alice on `branch`, carrying a chosen `CSeq`.
    fn rsp(at_us: u64, status: u16, cseq: u32, method: &str, branch: &str) -> Msg {
        Msg {
            at_us,
            src: BOB.to_string(),
            dst: ALICE.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: method.to_string(),
            from_tag: Some("fa".to_string()),
            to_tag: Some("btag".to_string()),
            via_branch: Some(branch.to_string()),
            head: None,
            body: None,
        }
    }

    fn on_call(mut m: Msg, call_id: &str) -> Msg {
        m.call_id = call_id.to_string();
        m
    }

    fn from(mut m: Msg, from_tag: &str) -> Msg {
        m.from_tag = Some(from_tag.to_string());
        m
    }

    /// The same message again on the wire — what the §17.2 mark states, and
    /// what the rule's own fold must reach without it.
    fn again(mut m: Msg) -> Msg {
        m.repeat = true;
        m
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// The violations of one rule, in observation order.
    fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        eval(rule, msgs).into_iter().filter(Finding::violated).collect()
    }

    fn order(msgs: &[Msg]) -> Vec<Finding> {
        hits(&CseqInDialogOrder, msgs)
    }

    // ── CseqInDialogOrder (RFC 3261 §12.2.1.1) ──────────────────────────────

    /// INVITE(1) / OPTIONS(2) / BYE(3) on distinct branches: contiguous, none
    /// reused. Every accounted request is an occasion, and each is compliant.
    #[test]
    fn a_contiguous_dialog_run_is_clean() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            options(2_000, 2),
            in_dialog(3_000, "BYE", 3, "z9hG4bK-b"),
        ];
        let all = eval(&CseqInDialogOrder, &msgs);
        assert_eq!(all.len(), 3, "one occasion per accounted request: {all:?}");
        assert!(all.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{all:?}");
        assert_eq!(all[0].emitter, ALICE, "the UAC that generated the stream is charged");
        assert_eq!(all[0].taker, BOB, "read at the endpoint that took it");
    }

    /// A retransmission repeats its CSeq *and* its top-Via branch: one
    /// transaction, folded away, and no second occasion.
    #[test]
    fn a_retransmission_is_one_transaction() {
        let msgs = [options(1_000, 2), options(2_000, 2)];
        let all = eval(&CseqInDialogOrder, &msgs);
        assert_eq!(all.len(), 1, "the repeat folds into the first: {all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant));
    }

    /// The fold is the rule's OWN and needs no §17.2 mark: the same wire with
    /// the repeat marked decides identically, so the mark can neither add nor
    /// remove a verdict.
    #[test]
    fn the_repeat_mark_changes_no_verdict() {
        let unmarked = eval(&CseqInDialogOrder, &[options(1_000, 2), options(2_000, 2)]);
        let marked = eval(&CseqInDialogOrder, &[options(1_000, 2), again(options(2_000, 2))]);
        assert_eq!(unmarked.len(), marked.len());
        assert!(marked.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{marked:?}");
    }

    /// The production bug: an OPTIONS keepalive at CSeq 2, then a BYE that
    /// reuses CSeq 2 on a NEW branch — a new transaction whose dialog CSeq
    /// never advanced, which §17.2.3 cannot fold away.
    #[test]
    fn a_reuse_on_a_fresh_branch_is_violated() {
        let msgs = [options(1_000, 2), in_dialog(2_000, "BYE", 2, "z9hG4bK-y")];
        let f = order(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::CseqInDialogOrder);
        assert_eq!(f[0].emitter, ALICE);
        assert_eq!(f[0].taker, BOB);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the REUSING request");
        let Decision::Violated(Evidence::CseqReused {
            cseq,
            method,
            reuse_branch,
            spent_branch,
            spent_msg,
            to_tag,
            ..
        }) = &f[0].decision
        else {
            panic!("reuse evidence: {:?}", f[0].decision)
        };
        assert_eq!((*cseq, method.as_str()), (2, "BYE"));
        assert_eq!(spent_branch.as_str(), "z9hG4bK-2", "the transaction that spent it first");
        assert_eq!(reuse_branch.as_str(), "z9hG4bK-y", "a FRESH branch, so not a retransmission");
        assert_eq!(*spent_msg, 0);
        assert_eq!(to_tag.as_str(), "btag");
    }

    /// The stale-snapshot takeover: the primary keepalives 2 then 3, the
    /// survivor re-originates OPTIONS 2 on a fresh branch. The REUSE is the
    /// violation; arriving "after" CSeq 3 is not judged on its own.
    #[test]
    fn a_stale_takeover_reusing_a_spent_cseq_is_violated() {
        let msgs =
            [options(1_000, 2), options(2_000, 3), in_dialog(3_000, "OPTIONS", 2, "z9hG4bK-stale")];
        let f = order(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Violated(Evidence::CseqReused { .. })));
        assert_eq!(f[0].anchor, 2);
    }

    /// §12.2.1.1 constrains what the UAC GENERATES, not what a recording
    /// observes: a NOTIFY(3) dropped and recovered after the BYE(4) leaves no
    /// hole, so the set `{2,3,4}` is contiguous and the reordering is clean.
    #[test]
    fn a_request_recovered_out_of_order_leaves_no_hole() {
        let msgs = [
            in_dialog(1_000, "INVITE", 2, "z9hG4bK-ri"),
            in_dialog(2_000, "BYE", 4, "z9hG4bK-b"),
            in_dialog(3_000, "NOTIFY", 3, "z9hG4bK-n"),
        ];
        assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
    }

    /// A +2 jump between two new requests on one dialog breaks
    /// increment-by-exactly-one even though it strictly increases.
    #[test]
    fn a_skipped_number_is_violated() {
        let msgs = [options(1_000, 2), options(2_000, 4)];
        let f = order(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, method, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*cseq, *prior_cseq, method.as_str()), (4, 2, "OPTIONS"));
        assert_eq!(f[0].anchor, 1, "the request that skipped ahead");
    }

    /// ACK and CANCEL reuse the CSeq of the request they answer: exempt, and
    /// they never advance the dialog's own sequence.
    #[test]
    fn ack_and_cancel_reusing_a_cseq_are_exempt() {
        let acked = [
            req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
            in_dialog(2_000, "ACK", 1, "z9hG4bK-a1"),
            in_dialog(3_000, "INVITE", 2, "z9hG4bK-i2"),
            in_dialog(4_000, "ACK", 2, "z9hG4bK-a2"),
        ];
        assert!(order(&acked).is_empty(), "{:?}", order(&acked));

        let cancelled = [
            req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
            req(2_000, "CANCEL", 1, "z9hG4bK-c1", None),
        ];
        assert!(order(&cancelled).is_empty(), "{:?}", order(&cancelled));
    }

    /// A different Call-ID, and the other direction of one dialog (a different
    /// From tag), each own an independent CSeq space — no cross-stream alias.
    #[test]
    fn separate_streams_do_not_alias() {
        let msgs =
            [options(1_000, 5), on_call(options(2_000, 1), "c2"), from(options(3_000, 1), "fb")];
        assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
    }

    /// Forking (§12.1.2): ONE INVITE creates TWO early dialogs sharing the
    /// Call-ID and From tag but carrying distinct callee To tags, each with its
    /// own CSeq space seeded from the INVITE — so BOTH first PRACKs at
    /// `INVITE + 1` are correct, and the confirmed fork then advances by one in
    /// ITS dialog.
    #[test]
    fn two_forks_at_one_cseq_are_distinct_dialogs() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            req(2_000, "PRACK", 2, "z9hG4bK-p1", Some("fork1")),
            req(3_000, "PRACK", 2, "z9hG4bK-p2", Some("fork2")),
            req(4_000, "BYE", 3, "z9hG4bK-b", Some("fork1")),
        ];
        assert!(order(&msgs).is_empty(), "{:?}", order(&msgs));
    }

    /// A forked dialog's FIRST in-dialog request must be exactly the
    /// dialog-creating INVITE's CSeq + 1: a PRACK at 3 off an INVITE at 1 skips
    /// a number.
    #[test]
    fn a_fork_first_request_off_the_invite_baseline_is_violated() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            req(2_000, "PRACK", 3, "z9hG4bK-p", Some("fork1")),
        ];
        let f = order(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*cseq, *prior_cseq), (3, 1), "measured off the INVITE anchor");
    }

    /// The per-To-tag split hides a first in-dialog request that reuses the
    /// INVITE's own number — the anchor test is what catches it, and a CSeq
    /// BELOW the anchor (CSeq 0 always) the same way.
    #[test]
    fn a_first_request_at_or_below_the_anchor_never_advanced() {
        let reuse = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            req(2_000, "INFO", 1, "z9hG4bK-info", Some("btag")),
        ];
        let f = order(&reuse);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*cseq, *prior_cseq), (1, 1), "at the anchor: it did not advance");

        let regressed = [
            req(1_000, "INVITE", 5, "z9hG4bK-i", None),
            req(2_000, "BYE", 3, "z9hG4bK-b", Some("btag")),
        ];
        assert_eq!(order(&regressed).len(), 1, "a CSeq that went backwards");

        let zero = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            req(2_000, "INFO", 0, "z9hG4bK-info", Some("btag")),
        ];
        assert_eq!(order(&zero).len(), 1, "CSeq 0 is below any ≥1 anchor");
    }

    /// A deferred-auth §22.2 retry leaves BOTH attempts dialog-creating (the
    /// 401 established no dialog), so the empty-To-tag bucket holds `{1,2}`.
    /// The confirmed dialog anchors on the attempt that ESTABLISHED it (2) —
    /// folding the abandoned attempt (1) would fabricate a `[1,3]` gap — while
    /// a real skip past the establishing attempt still surfaces.
    #[test]
    fn an_auth_retry_anchors_on_the_establishing_attempt() {
        let clean = [
            req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
            req(2_000, "ACK", 1, "z9hG4bK-a1", Some("btag")),
            req(3_000, "INVITE", 2, "z9hG4bK-i2", None),
            req(4_000, "ACK", 2, "z9hG4bK-a2", Some("btag")),
            req(5_000, "BYE", 3, "z9hG4bK-b", Some("btag")),
        ];
        assert!(order(&clean).is_empty(), "{:?}", order(&clean));

        let skipped = [
            req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
            req(2_000, "INVITE", 2, "z9hG4bK-i2", None),
            req(3_000, "BYE", 4, "z9hG4bK-b", Some("btag")),
        ];
        let f = order(&skipped);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::CseqNotContiguous { cseq, prior_cseq, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*cseq, *prior_cseq), (4, 2), "the +2 gap off the RESENT INVITE");
    }

    /// A request the vantage carried no branch for cannot be folded, so its
    /// numbers still join the dialog's set — the silence the old engine kept:
    /// contiguity is judged, and only a same-number pair goes unjudged because
    /// nothing tells the two transactions apart.
    #[test]
    fn a_branchless_request_still_joins_the_set() {
        let branchless = |mut m: Msg| {
            m.via_branch = None;
            m
        };
        let gap = [branchless(options(1_000, 2)), branchless(options(2_000, 4))];
        assert_eq!(order(&gap).len(), 1, "the gap is still judged");

        let pair = [branchless(options(1_000, 2)), branchless(options(2_000, 2))];
        assert!(order(&pair).is_empty(), "nothing separates the two transactions");
    }

    // ── ResponseCseqMatchesTransaction (RFC 3261 §8.1.3.5) ──────────────────

    /// A response copying its request's CSeq verbatim is the obligation met.
    #[test]
    fn a_response_copying_its_requests_cseq_is_compliant() {
        let msgs = [
            in_dialog(1_000, "PRACK", 2, "z9hG4bK-brP"),
            rsp(2_000, 200, 2, "PRACK", "z9hG4bK-brP"),
        ];
        let all = eval(&ResponseCseqMatchesTransaction, &msgs);
        assert_eq!(all.len(), 1, "one occasion, the response: {all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    }

    /// The forking corruption: the INVITE is CSeq 1 on branch brI, and a
    /// `200 (INVITE)` carrying the PRACK's number comes back on that branch —
    /// no request on that transaction had it.
    #[test]
    fn a_response_cseq_no_request_carried_is_violated() {
        let msgs = [
            in_dialog(1_000, "INVITE", 1, "z9hG4bK-brI"),
            rsp(2_000, 200, 2, "INVITE", "z9hG4bK-brI"),
        ];
        let f = hits(&ResponseCseqMatchesTransaction, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, BOB, "the endpoint that sent the response is charged");
        assert_eq!(f[0].taker, ALICE);
        let Decision::Violated(Evidence::ResponseCseqUnmatched {
            status,
            response_cseq,
            response_method,
            txn_cseqs,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*status, *response_cseq, response_method.as_str()), (200, 2, "INVITE"));
        assert_eq!(txn_cseqs.as_slice(), ["1 INVITE"], "what the transaction did carry");
        assert_eq!(branch.as_str(), "z9hG4bK-brI");
    }

    /// A CANCEL shares its INVITE's branch (§9.1), so one transaction key
    /// carries two CSeq methods — and each side's own 200 matches one of them.
    #[test]
    fn a_cancel_sharing_its_invites_branch_matches_either_cseq() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-c", None),
            req(2_000, "CANCEL", 1, "z9hG4bK-c", None),
            rsp(3_000, 200, 1, "CANCEL", "z9hG4bK-c"),
            rsp(4_000, 487, 1, "INVITE", "z9hG4bK-c"),
        ];
        assert!(
            hits(&ResponseCseqMatchesTransaction, &msgs).is_empty(),
            "{:?}",
            hits(&ResponseCseqMatchesTransaction, &msgs)
        );
    }

    /// A branch whose requests this vantage never carried settles nothing, and
    /// neither does a response with no branch at all — undecidable, never a
    /// guess, and counted so the conservatism shows.
    #[test]
    fn a_response_the_vantage_cannot_place_is_undecidable() {
        let unseen = [rsp(1_000, 200, 7, "OPTIONS", "z9hG4bK-unseen")];
        let f = eval(&ResponseCseqMatchesTransaction, &unseen);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);

        let branchless = {
            let mut m = rsp(1_000, 200, 7, "OPTIONS", "z9hG4bK-x");
            m.via_branch = None;
            [m]
        };
        let f = eval(&ResponseCseqMatchesTransaction, &branchless);
        assert!(
            matches!(f[0].decision, Decision::Undecidable("no via branch at this vantage")),
            "{:?}",
            f[0].decision
        );
    }

    // ── AckCseqMatchesInvite (RFC 3261 §13.2.2.4) ───────────────────────────

    /// Each ACK reuses its own INVITE's CSeq — the obligation met, twice.
    #[test]
    fn an_ack_reusing_its_invites_cseq_is_compliant() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-i1", None),
            in_dialog(2_000, "ACK", 1, "z9hG4bK-a1"),
            in_dialog(3_000, "INVITE", 4, "z9hG4bK-i2"),
            in_dialog(4_000, "ACK", 4, "z9hG4bK-a2"),
        ];
        let all = eval(&AckCseqMatchesInvite, &msgs);
        assert_eq!(all.len(), 2, "one occasion per ACK: {all:?}");
        assert!(all.iter().all(|f| matches!(f.decision, Decision::Compliant)), "{all:?}");
    }

    /// The bug: an early PRACK/UPDATE advanced the dialog CSeq, so the 2xx ACK
    /// lands at a number no INVITE used and a real UAS cannot match it to the
    /// INVITE server transaction.
    #[test]
    fn an_ack_on_a_number_no_invite_used_is_violated() {
        let msgs =
            [req(1_000, "INVITE", 1, "z9hG4bK-i", None), in_dialog(2_000, "ACK", 3, "z9hG4bK-a")];
        let f = hits(&AckCseqMatchesInvite, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, ALICE, "the endpoint that sent the ACK is charged");
        assert_eq!(f[0].taker, BOB);
        let Decision::Violated(Evidence::AckCseqUnmatched {
            ack_cseq, invite_cseqs, from_tag, ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*ack_cseq, 3);
        assert_eq!(invite_cseqs.as_slice(), [1], "the INVITE it owed");
        assert_eq!(from_tag.as_str(), "fa");
    }

    /// An ACK is judged against the INVITEs the stream had carried BY THEN, so
    /// a stream with none yet settles nothing.
    #[test]
    fn an_ack_before_any_invite_is_undecidable() {
        let f = eval(&AckCseqMatchesInvite, &[in_dialog(1_000, "ACK", 9, "z9hG4bK-a")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(
                f[0].decision,
                Decision::Undecidable("no INVITE on this stream at this vantage")
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// The ACK rule reads every copy the taker took: §12.2.1.1's fold is not
    /// its fold, so a retransmitted bad ACK is charged as often as it lands.
    #[test]
    fn a_retransmitted_ack_is_judged_every_time() {
        let msgs = [
            req(1_000, "INVITE", 1, "z9hG4bK-i", None),
            in_dialog(2_000, "ACK", 3, "z9hG4bK-a"),
            again(in_dialog(3_000, "ACK", 3, "z9hG4bK-a")),
        ];
        assert_eq!(hits(&AckCseqMatchesInvite, &msgs).len(), 2, "one per arrival");
    }
}
