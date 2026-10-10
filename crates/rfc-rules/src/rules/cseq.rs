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
//! **An out-of-dialog request is told from its UAC's reuse by its bottom
//! Via.** Every proxy pushes its Via above the ones it received (§16.6 step 8),
//! so the bottom Via is the originator's as it sent the request. A second
//! transaction on a spent number with no To tag is judged against the first
//! copy's bottom Via:
//!
//!   - the same sent-by and branch: the same request reaching the taker by
//!     another path — a spiral (§16.3) or a merged fork (§8.2.2.2) — and it
//!     records nothing;
//!   - another sent-by host: another element originated it (a transparent
//!     B2BUA's outgoing leg keeps the caller's Call-ID, From tag and CSeq), and
//!     it is not this UAC's reuse. The port is not compared: a UA on TCP may
//!     name a new ephemeral port per connection (§18.1.1);
//!   - the same host under a new branch: the UAC's own new transaction on a
//!     spent number, a reuse (§8.1.3.5; §22.2 makes the increment a MUST for a
//!     credentials retry).
//!
//! The in-dialog buckets read the same way: a Record-Routed route set that
//! spirals (§12.2, §16.3) hands one in-dialog request to an element twice. A
//! copy whose Via stack the vantage did not carry, or whose bottom Via names
//! no branch, proves neither, and the reuse reading stands. An element that
//! copies the caller's Vias under its own breaks §8.1.1.7 (a UA inserts one
//! Via) and can mask its own reuse as the caller's request.
//!
//! **A forwarded request is marked relayed on evidence.** A proxy forwards the
//! CSeq unchanged (§16.6), so a finding on a request is `relayed` only where
//! the view carries the same request from another emitter nearer the origin:
//! the same Call-ID, bottom Via (sent-by and branch), CSeq number and method,
//! under a shorter Via stack. Any hop counts, since a forwarding box may wear
//! another address on each interface, and view order does not, since a view
//! merged from several captures can carry the forwarded copy first. The live
//! audit judges each bind's view alone, which never carries the request on its
//! way into the forwarder, so relay attribution applies to capture views.
//! Without that evidence — a one-sided view, or a CSeq the forwarder
//! rewrote — the emitter is charged as the originator.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sip_message::header::SentBy;
use sip_message::sniff::{self, OriginVia};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

crate::registry::rules! {
    /// RFC 3261 §12.2.1.1: within a dialog a UAC increments the CSeq by exactly one per new request
    /// (ACK and CANCEL excepted). Charges that UAC; read at the endpoint that TOOK the stream.
    CseqInDialogOrder = "cseq-in-dialog-order";
    /// RFC 3261 §8.1.3.5: a response copies its request's CSeq verbatim. Charges the UAS that sent
    /// the response.
    ResponseCseqMatchesTransaction = "response-cseq-matches-transaction";
    /// RFC 3261 §13.2.2.4: an ACK reuses the CSeq number of the INVITE it acknowledges. Charges the
    /// UAC that sent the ACK.
    AckCseqMatchesInvite = "ack-cseq-matches-invite";
}

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
                        relayed: seen.relayed(req),
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
    /// Each request the view carried, named by what a forwarder keeps (§16.6),
    /// → every (emitter, Via stack depth) it was carried with.
    copies: HashMap<ForwardedKey<'a>, Vec<(&'a str, usize)>>,
}

/// A request as every hop that forwards it carries it: Call-ID, bottom Via
/// sent-by and branch, CSeq number and method (uppercased).
type ForwardedKey<'a> = (&'a str, SentBy, String, u32, String);

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
    /// The request's bottom Via, where the vantage carried its head: the hop
    /// that originated it.
    origin: Option<OriginVia>,
    /// The request as a forwarder keeps it, where a bottom Via names it.
    forwarded: Option<ForwardedKey<'a>>,
}

impl Request<'_> {
    /// Whether `copy`, a later request on this one's number, is
    /// its originator's own reuse rather than this request by another path or
    /// another originator's — read off the two bottom Vias (see the module
    /// doc). Without both Vias, or a bottom branch, the reuse reading stands.
    fn reused_by(&self, copy: &Request<'_>) -> bool {
        let (Some(first), Some(again)) = (&self.origin, &copy.origin) else { return true };
        if !first.sent_by.as_borrowed().same_host(&again.sent_by.as_borrowed()) {
            return false;
        }
        first.branch.is_none() || first.branch != again.branch
    }

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

    /// `msg`'s request as a forwarder keeps it, or `None` where no bottom Via
    /// with a branch names it across hops.
    fn forwarded_key(msg: &'a Msg, origin: Option<&OriginVia>) -> Option<ForwardedKey<'a>> {
        let origin = origin?;
        Some((
            msg.call_id.as_str(),
            origin.sent_by.clone(),
            origin.branch.clone()?,
            msg.cseq,
            msg.cseq_method.to_ascii_uppercase(),
        ))
    }

    /// Whether the view carries `req` from another emitter nearer the origin
    /// (see the module doc).
    fn relayed(&self, req: &Request<'_>) -> bool {
        let (Some(key), Some(origin)) = (&req.forwarded, &req.origin) else { return false };
        self.copies.get(key).is_some_and(|copies| {
            copies.iter().any(|&(emitter, depth)| emitter != req.emitter && depth < origin.depth)
        })
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
        let origin = msg.head.as_deref().and_then(sniff::bottom_via);
        let forwarded = Self::forwarded_key(msg, origin.as_ref());
        if let (Some(key), Some(origin)) = (&forwarded, &origin) {
            self.copies.entry(key.clone()).or_default().push((msg.src.as_str(), origin.depth));
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
        let to_tag = msg.to_tag.as_deref().unwrap_or_default();
        let req = Request {
            msg: mi,
            hop: msg.hop,
            ts_us: msg.at_us,
            cseq: msg.cseq,
            method,
            branch: branch.unwrap_or_default(),
            emitter: msg.src.as_str(),
            from_tag,
            origin,
            forwarded,
        };
        let dialog = stream.dialogs.entry(to_tag).or_default();
        match dialog.by_cseq.get(&req.cseq) {
            None => {
                dialog.by_cseq.insert(req.cseq, req);
            }
            // Another path for the same request, or another originator's
            // request: not this UAC's reuse (see the module doc).
            Some(first) if !first.reused_by(&req) => {}
            // A DIFFERENT transaction carried a CSeq the dialog already spent →
            // the dialog CSeq failed to increment. Same branch, different method
            // is one transaction's shape restated, and records nothing new.
            Some(first) if first.branch != req.branch => dialog.reuses.push(req),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests;
