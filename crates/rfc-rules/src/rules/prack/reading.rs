//! The one pass over the wire every rule of the family reads.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::header::{HeaderValue, RAck, Supported};
use sip_message::{sniff, SipStr};

use crate::wire::{Kind, Msg};

use super::records::{
    Answer, Invite, ObligationKey, Prack, Rack, Reliable, Served, Trying, TxnKey,
};

/// (endpoint, call, early dialog, INVITE CSeq number, `RSeq`): one PRACK
/// ledger's key.
type PrackKey<'a> = (&'a str, &'a str, &'a str, u32, u64);

/// What one view's messages say about the 100rel obligations on it.
#[derive(Debug, Default)]
pub(super) struct Reading<'a> {
    /// (endpoint, INVITE CSeq number) → the INVITE that endpoint SENT.
    pub(super) invite: BTreeMap<(&'a str, u32), Invite>,
    /// (endpoint, Call-ID, INVITE CSeq number) → when that endpoint SENT the
    /// INVITE: the per-call reading the obligation's hop test takes.
    pub(super) opened: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// (Call-ID, From tag, INVITE CSeq number) → the first time that INVITE
    /// crossed any vantage of the view. An endpoint whose own emission is not
    /// that first one relayed an INVITE somebody else opened. The From tag
    /// keeps the two directions of one dialog apart: each side numbers its own
    /// CSeq space (RFC 3261 §12.2.1.1).
    pub(super) invite_first_us: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// The reliable provisionals taken, one entry per obligation.
    pub(super) owed: BTreeMap<ObligationKey<'a>, Reliable<'a>>,
    /// (endpoint, To tag) → the reliable provisionals that endpoint SENT on
    /// that early dialog, in emission order.
    pub(super) emitted: BTreeMap<(&'a str, &'a str), Vec<Reliable<'a>>>,
    /// (endpoint, To tag) → the reliable provisionals that endpoint TOOK on
    /// that early dialog, in arrival order.
    pub(super) taken: BTreeMap<(&'a str, &'a str), Vec<Reliable<'a>>>,
    /// (INVITE CSeq number, To tag, RSeq) → when some endpoint on this view
    /// first PRACKed it.
    pub(super) acked_at: BTreeMap<(u32, &'a str, u64), u64>,
    /// Every PRACK the view carried, in observation order.
    pub(super) pracks: Vec<Prack<'a>>,
    /// (sender, INVITE CSeq number, To tag, RSeq) → the LAST time that
    /// endpoint SENT that provisional. A taker that sent it after taking it
    /// passed it on; a copy it only received again is the responder's
    /// retransmission, which forwards nothing.
    pub(super) propagated: BTreeMap<(&'a str, u32, &'a str, u64), u64>,
    /// A PRACK this endpoint sent whose `RAck` will not parse: every
    /// obligation of that endpoint on this view becomes undecidable.
    pub(super) unreadable_prack: BTreeSet<&'a str>,
    /// (UAC, Call-ID, INVITE CSeq number) → when the INVITE's first final
    /// reached the UAC. A CANCEL ends nothing (RFC 3262 §4).
    pub(super) final_at: BTreeMap<(&'a str, &'a str, u32), u64>,
    /// (UAC, Call-ID, dialog's To tag) → when the responder's BYE on that
    /// dialog first reached the UAC: before the INVITE's final, it ended the
    /// early dialog, and nothing rides it after (RFC 3261 §15.1.2).
    pub(super) byed_at: BTreeMap<(&'a str, &'a str, &'a str), u64>,
    /// The last timestamp on this view's own stream: after it, an absence is
    /// the observation's rather than the wire's.
    pub(super) last_ts_us: u64,

    // ── the server-transaction half ────────────────────────────────────────
    /// (UAS, call, branch) → the FIRST request that endpoint took on the
    /// transaction: what every response it sends there answers.
    pub(super) served: BTreeMap<TxnKey<'a>, Served<'a>>,
    /// (UAS, call, branch) → the responses that endpoint SENT on the
    /// transaction, in emission order.
    pub(super) answers: BTreeMap<TxnKey<'a>, Vec<Answer<'a>>>,
    /// (endpoint, call) → every `RSeq` that endpoint TOOK on a non-100
    /// provisional, reliable or not: what a PRACK arriving at it can name.
    pub(super) rseqs_taken: BTreeMap<(&'a str, &'a str), BTreeSet<u64>>,
    /// (endpoint, call) → the `100 Trying`s that endpoint took carrying BOTH
    /// `Require: 100rel` and an `RSeq` — the 100rel §4 has a UAC ignore.
    pub(super) trying_100rel: BTreeMap<(&'a str, &'a str), Vec<Trying<'a>>>,
    /// (taker, call, early dialog, INVITE CSeq number, `RSeq`) → when that
    /// endpoint FIRST took a PRACK naming it. The To tag keeps forks apart:
    /// each numbers its own `RSeq`s.
    pracked_to: BTreeMap<PrackKey<'a>, u64>,
    /// The same, keyed by the PRACK's sender: the PRACK goes to the early
    /// dialog's remote target, the UAS's Contact, which need not be the
    /// address the provisional came from (RFC 3262 §4).
    pracked_by: BTreeMap<PrackKey<'a>, u64>,
}

impl<'a> Reading<'a> {
    pub(super) fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.last_ts_us = seen.last_ts_us.max(msg.at_us);
            // A repeat is not a fresh ACT, but it is still a fact on the wire
            // ([`crate::wire`]): it RESTATES the request a transaction serves,
            // and a PRACK that is a repeat still MEETS what it acknowledges.
            // Only the occasion ledgers below skip it.
            seen.restated(mi, msg);
            if msg.repeat {
                continue;
            }
            seen.absorb(mi, msg);
        }
        seen
    }

    /// The facts a retransmission restates rather than re-enacts: the request
    /// each endpoint serves on a branch, and the PRACKs it has taken.
    ///
    /// Taking repeats here is load-bearing, not tidiness: where a vantage drops
    /// the first copy of a message and keeps the retransmission, that copy is
    /// marked a repeat and is the ONLY statement of the fact the view carries.
    fn restated(&mut self, mi: usize, msg: &'a Msg) {
        let Kind::Request { method } = &msg.kind else { return };
        let head = msg.head.as_deref();
        let call = msg.call_id.as_str();
        if let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) {
            self.served.entry(TxnKey { uas: msg.dst.as_str(), call_id: call, branch }).or_insert(
                Served {
                    msg: mi,
                    ts_us: msg.at_us,
                    method: method.as_str(),
                    client: msg.src.as_str(),
                    requires_100rel: head.is_some_and(sniff::require_has_100rel),
                    offers_100rel: head.is_some_and(offers_100rel),
                    dialog: msg.to_tag.as_deref().unwrap_or_default(),
                    rack: head.and_then(rack_triple).map(|(rseq, cseq, method)| Rack {
                        rseq,
                        cseq,
                        on_invite: method == "INVITE",
                    }),
                },
            );
        }
        if method.eq_ignore_ascii_case("PRACK") {
            if let Some((rseq, cseq)) = head.and_then(rack_of) {
                let dialog = msg.to_tag.as_deref().unwrap_or_default();
                for (ledger, endpoint) in
                    [(&mut self.pracked_to, &msg.dst), (&mut self.pracked_by, &msg.src)]
                {
                    let at = ledger
                        .entry((endpoint.as_str(), call, dialog, cseq, rseq))
                        .or_insert(msg.at_us);
                    *at = (*at).min(msg.at_us);
                }
            }
        }
    }

    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        let head = msg.head.as_deref();
        self.absorb_transaction(mi, msg, head);
        if msg.is_request("INVITE") {
            let call = msg.call_id.as_str();
            let from_tag = msg.from_tag.as_deref().unwrap_or_default();
            let first = self.invite_first_us.entry((call, from_tag, msg.cseq)).or_insert(msg.at_us);
            *first = (*first).min(msg.at_us);
            self.opened.entry((msg.src.as_str(), call, msg.cseq)).or_insert(msg.at_us);
            self.invite
                .entry((msg.src.as_str(), msg.cseq))
                .or_insert(Invite { offers_100rel: head.is_some_and(offers_100rel) });
        } else if msg.is_request("BYE") {
            let dialog = msg.from_tag.as_deref().unwrap_or_default();
            let at = self
                .byed_at
                .entry((msg.dst.as_str(), msg.call_id.as_str(), dialog))
                .or_insert(msg.at_us);
            *at = (*at).min(msg.at_us);
        } else if msg.is_request("PRACK") {
            let rack = head.and_then(rack_of);
            let dialog = msg.to_tag.as_deref().unwrap_or_default();
            match rack {
                Some((rseq, seq)) => {
                    self.acked_at.entry((seq, dialog, rseq)).or_insert(msg.at_us);
                }
                None => {
                    self.unreadable_prack.insert(msg.src.as_str());
                }
            }
            self.pracks.push(Prack {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                src: msg.src.as_str(),
                dst: msg.dst.as_str(),
                call_id: msg.call_id.as_str(),
                dialog,
                rack,
                rack_triple: head.and_then(rack_triple),
            });
        } else if let Some(status) = msg.status() {
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            if status >= 200 {
                self.answered_finally(msg.dst.as_str(), msg.call_id.as_str(), msg.cseq, msg.at_us);
                return;
            }
            let Some(head) = head else { return };
            // §3: 100 is never reliable, and the marker is BOTH headers.
            if status < 101 || !sniff::require_has_100rel(head) {
                return;
            }
            let Some(rseq) = sniff::rseq_of(head) else { return };
            let dialog = msg.to_tag.as_deref().unwrap_or_default();
            let last = self
                .propagated
                .entry((msg.src.as_str(), msg.cseq, dialog, rseq))
                .or_insert(msg.at_us);
            *last = (*last).max(msg.at_us);
            let common = Reliable {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                status,
                cseq: msg.cseq,
                rseq,
                peer: msg.src.as_str(),
            };
            self.owed
                .entry(ObligationKey {
                    uac: msg.dst.as_str(),
                    call: msg.call_id.as_str(),
                    cseq: msg.cseq,
                    dialog,
                    rseq,
                })
                .or_insert(common);
            self.taken.entry((msg.dst.as_str(), dialog)).or_default().push(common);
            self.emitted
                .entry((msg.src.as_str(), dialog))
                .or_default()
                .push(Reliable { peer: msg.dst.as_str(), ..common });
        }
    }

    /// The occasion half of the server-transaction walk: the responses each
    /// endpoint ANSWERED with on a branch, and the per-call `RSeq` facts a
    /// PRACK is weighed against. Repeats never reach it — a retransmission is
    /// not a second answer — and the facts a repeat restates are
    /// [`Reading::restated`]'s.
    ///
    /// **A message with no top-Via branch keys no transaction.** The branch IS
    /// the pairing; without one, no response can be matched to the request it
    /// answers, and the reading states nothing rather than guessing.
    fn absorb_transaction(&mut self, mi: usize, msg: &'a Msg, head: Option<&[u8]>) {
        let call = msg.call_id.as_str();
        let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
        match &msg.kind {
            Kind::Request { .. } => {}
            Kind::Response { status } => {
                let status = *status;
                if let Some(branch) = branch {
                    self.answers
                        .entry(TxnKey { uas: msg.src.as_str(), call_id: call, branch })
                        .or_default()
                        .push(Answer {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            status,
                            cseq: msg.cseq,
                            taker: msg.dst.as_str(),
                            method: msg.cseq_method.as_str(),
                            requires_100rel: head.is_some_and(sniff::require_has_100rel),
                            rejects_100rel: head
                                .is_some_and(|h| lists_option(h, "Unsupported", "100rel")),
                            rseq: head.and_then(sniff::rseq_of),
                            to_tag: msg.to_tag.as_deref().unwrap_or_default(),
                            has_body: head.is_some_and(sniff::has_body),
                            readable: head.is_some(),
                        });
                }
                let Some(rseq) = head.and_then(sniff::rseq_of) else { return };
                if status == 100 {
                    // §4: a 100 is never reliable, so an RSeq on one is a
                    // temptation rather than an obligation.
                    if head.is_some_and(sniff::require_has_100rel) {
                        self.trying_100rel.entry((msg.dst.as_str(), call)).or_default().push(
                            Trying {
                                msg: mi,
                                ts_us: msg.at_us,
                                cseq: msg.cseq,
                                rseq,
                                peer: msg.src.as_str(),
                            },
                        );
                    }
                } else if (101..200).contains(&status) {
                    self.rseqs_taken.entry((msg.dst.as_str(), call)).or_default().insert(rseq);
                }
            }
        }
    }

    /// The responses one endpoint sent on one server transaction, in emission
    /// order; empty where this vantage carried none.
    pub(super) fn answers_on(&self, key: &TxnKey<'a>) -> &[Answer<'a>] {
        self.answers.get(key).map_or(&[][..], Vec::as_slice)
    }

    /// The `(RSeq, INVITE CSeq-num)` pairs sent reliably on the `dialog` early
    /// dialog of `call` before `ts_us`, by `uas` or to `uac` — what a PRACK
    /// from `uac` arriving at `uas` then could legitimately name (§3, §7.2).
    /// By `uas`: the PRACK may come straight from a UAC whose provisionals
    /// came through a proxy. To `uac`: the PRACK reaches the UAS at its
    /// Contact, which need not be the address its provisionals came from (§4).
    /// The To tag keeps forks apart: each numbers its own `RSeq`s.
    pub(super) fn reliably_sent_before(
        &self,
        uas: &str,
        uac: &str,
        call: &str,
        dialog: &str,
        ts_us: u64,
    ) -> BTreeSet<(u64, u32)> {
        self.answers
            .iter()
            .filter(|(k, _)| k.call_id == call)
            .flat_map(|(k, list)| list.iter().map(move |a| (k, a)))
            .filter(|(k, a)| k.uas == uas || a.taker == uac)
            .map(|(_, a)| a)
            .filter(|a| a.ts_us < ts_us && a.reliable_1xx() && a.to_tag == dialog)
            .filter_map(|a| a.rseq.map(|rseq| (rseq, a.cseq)))
            .collect()
    }

    /// Whether a PRACK naming `(INVITE CSeq, RSeq)` on that early dialog of
    /// `call` was taken by `uas`, or sent by `uac`, the endpoint the
    /// provisional went to, between the provisional and a later emission of
    /// `uas`: the PRACK may come straight from a UAC behind a proxy, or reach
    /// the UAS at a Contact that is not the address it answers from (§4).
    pub(super) fn pracked_between(
        &self,
        (uas, uac): (&str, &str),
        (call, dialog, cseq, rseq): (&str, &str, u32, u64),
        after: u64,
        before: u64,
    ) -> bool {
        let to = self.pracked_to.get(&(uas, call, dialog, cseq, rseq));
        let by = self.pracked_by.get(&(uac, call, dialog, cseq, rseq));
        [to, by].into_iter().flatten().any(|at| *at > after && *at < before)
    }

    fn answered_finally(&mut self, uac: &'a str, call: &'a str, cseq: u32, ts_us: u64) {
        let at = self.final_at.entry((uac, call, cseq)).or_insert(ts_us);
        *at = (*at).min(ts_us);
    }

    /// The INVITE CSeq numbers `endpoint` opened on this view.
    pub(super) fn invites_opened_by(&self, endpoint: &str) -> Vec<u32> {
        self.invite.keys().filter(|(ep, _)| *ep == endpoint).map(|(_, cseq)| *cseq).collect()
    }

    pub(super) fn pracked_before(&self, r: &Reliable<'a>, dialog: &'a str, ts_us: u64) -> bool {
        self.acked_at.get(&(r.cseq, dialog, r.rseq)).is_some_and(|at| *at < ts_us)
    }

    /// The endpoint passed this provisional on rather than originating it: the
    /// same `RSeq` reached it earlier on this view.
    pub(super) fn relayed_provisional(
        &self,
        ep: &'a str,
        dialog: &'a str,
        r: &Reliable<'a>,
    ) -> bool {
        self.taken.get(&(ep, dialog)).is_some_and(|list| {
            list.iter().any(|q| q.cseq == r.cseq && q.rseq == r.rseq && q.ts_us < r.ts_us)
        })
    }

    /// The endpoint passed this PRACK on rather than originating it: the same
    /// `RAck` reached it earlier on this view.
    pub(super) fn relayed_prack(&self, p: &Prack<'a>) -> bool {
        p.rack.is_some_and(|rack| {
            self.pracks.iter().any(|q| q.dst == p.src && q.rack == Some(rack) && q.ts_us < p.ts_us)
        })
    }
}

/// Whether an INVITE offers `100rel`, in `Require` (RFC 3262 §3: the UAS MUST
/// then send reliably) or in `Supported` (it MAY).
fn offers_100rel(raw: &[u8]) -> bool {
    if sniff::require_has_100rel(raw) {
        return true;
    }
    ["Supported", "k"].iter().any(|name| {
        sniff::header_value(raw, name).is_some_and(|v| {
            Supported::parse(&SipStr::owned(&v)).is_ok_and(|s| s.contains("100rel"))
        })
    })
}

/// Whether the set-like header `name` lists `tag` (RFC 3261 §7.3.1: option
/// tags are case-insensitive, comma folds and repeated rows unioned).
fn lists_option(raw: &[u8], name: &str, tag: &str) -> bool {
    sniff::option_tags(raw, name).iter().any(|t| t.eq_ignore_ascii_case(tag))
}

/// The FULL `(response-num, CSeq-num, method)` a PRACK's `RAck` names
/// (RFC 3262 §7.2), the method ASCII-uppercased. Unlike [`rack_of`] it accepts
/// any method: what identifies one PRACK as the copy of another is the triple
/// its sender wrote, not the triple it should have written.
fn rack_triple(raw: &[u8]) -> Option<(u64, u32, String)> {
    let value = sniff::header_value(raw, "RAck")?;
    let rack = RAck::parse(&SipStr::owned(&value)).ok()?;
    Some((u64::from(rack.rseq()), rack.seq(), rack.method().as_str().to_ascii_uppercase()))
}

/// The `(RSeq, CSeq number)` a PRACK's `RAck` names, for an INVITE
/// (RFC 3262 §7.2). `None` for an absent, unparseable or non-INVITE `RAck` —
/// every one of which leaves the obligation undecidable rather than unmet.
fn rack_of(raw: &[u8]) -> Option<(u64, u32)> {
    let value = sniff::header_value(raw, "RAck")?;
    let rack = RAck::parse(&SipStr::owned(&value)).ok()?;
    rack.method()
        .as_str()
        .eq_ignore_ascii_case("INVITE")
        .then_some((u64::from(rack.rseq()), rack.seq()))
}
