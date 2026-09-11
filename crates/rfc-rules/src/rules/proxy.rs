//! RFC 3261 §16 — what a proxy on the path owes, read off the wire either side
//! of it. FIVE obligations:
//!
//!   - [`Proxy100TryingNotForwarded`] (§16.7 step 5) charges the hop that
//!     forwarded a downstream 100 it should have absorbed, read from the UAC's
//!     own side.
//!   - [`Proxy100WithinGrace`] (§16.7) charges the hop that took an INVITE and
//!     neither answered it promptly nor said it was trying.
//!   - [`NoTarget404`] (§16.3) charges the proxy itself: a request it resolved
//!     no target for is answered 404, not some other error.
//!   - [`StrictRouteRewriteHandled`] (§16.4) charges the proxy that took a
//!     strict-routed request and forwarded it without the Request-URI swap.
//!   - [`StrictRouteShuffleOnSend`] (§16.6 step 6) charges the hop whose own
//!     outbound request still states a strict route at the top of its Route
//!     set — the send-side twin of the rule above.
//!
//! **The 100-budget occasion is an INVITE TRANSACTION the endpoint opened**,
//! keyed `(Call-ID, CSeq number)`: the count of 100s it took is only meaningful
//! against the count of INVITEs it sent, and both are facts on the wire, so
//! every occasion is DECIDED — this rule has no absence to wait out and a
//! closed observation reads exactly like an open one.
//!
//! A transaction whose INVITE this vantage never carried opens no occasion: a
//! 100 counted against nothing settles nothing.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::branch::{BranchKey, BranchReading};
use super::Obligation;

/// **RFC 3261 §16.7 step 5 — a stateful proxy absorbs the downstream 100.** A
/// proxy generates its OWN 100 toward the UAC and does not forward the one the
/// next hop emits, so a UAC observes at most one 100 per INVITE it sent on the
/// transaction. A retransmitted INVITE is owed the server transaction's
/// replayed provisional (§17.2.1) — that copy is owed, not forwarded — so the
/// budget is one 100 per INVITE, and only a 100 in EXCESS of them is a
/// downstream copy that should have been absorbed.
///
/// The occasion is ONE INVITE transaction the endpoint opened. Charges the
/// endpoint that sent the excess 100 — the hop that forwarded what it should
/// have swallowed. The test UAC silently ignores the extra copy, so nothing
/// else catches it.
///
/// **One occasion per transaction, however many copies arrive.** A path that
/// forwards three stray 100s has one defect, and the finding names the whole
/// count rather than firing per copy.
pub struct Proxy100TryingNotForwarded;

impl Obligation for Proxy100TryingNotForwarded {
    fn id(&self) -> RuleId {
        RuleId::Proxy100TryingNotForwarded
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, txn) in &seen.txns {
            // An endpoint measured against an INVITE this vantage never saw it
            // send is measured against nothing.
            let Some(invite) = txn.first_invite else { continue };
            let head = |anchor, emitter: &str, decision| Finding {
                rule: RuleId::Proxy100TryingNotForwarded,
                emitter: emitter.to_string(),
                taker: key.taker.to_string(),
                cseq: key.cseq,
                relayed: false,
                anchor,
                decision,
            };
            let Some(excess) = txn.excess.filter(|_| txn.taken > txn.sent) else {
                out.push(head(invite, txn.peer, Decision::Compliant));
                continue;
            };
            out.push(head(
                excess.msg,
                excess.src,
                Decision::Violated(Evidence::ExtraTryingForwarded {
                    trying_msg: excess.msg,
                    trying_hop: excess.hop,
                    trying_ts_us: excess.ts_us,
                    trying_taken: txn.taken,
                    invites_sent: txn.sent,
                }),
            ));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The §16.7 grace: how long a hop may hold an INVITE before it owes the UAC a
/// 100 Trying, microseconds.
///
/// 200 ms, as §16.7 states it. The rules read microseconds while the live
/// vantage stamps milliseconds and bumps a tied stamp by one microsecond to keep
/// its view strictly ordered, so a Δ this rule computes can exceed the
/// millisecond Δ by at most the number of messages sharing one millisecond — a
/// bound that only ever matters within microseconds of exactly 200 ms.
pub const PROXY_100_GRACE_US: u64 = 200_000;

/// **RFC 3261 §16.7 — a hop that cannot answer promptly says it is trying.**
/// A stateful proxy relaying an INVITE emits its own 100 Trying toward the UAC,
/// so the caller's Timer A stops retransmitting into a path that has the request
/// in hand. §16.7 only obliges it where the final cannot be produced promptly:
/// a hop that relays a 486 in two milliseconds owes nobody a Trying.
///
/// The occasion is ONE INVITE the endpoint TOOK on a transaction. Charges that
/// endpoint; a 100 on the transaction discharges it, and so does any final
/// within [`PROXY_100_GRACE_US`] of the INVITE.
///
/// **Advisory and proxy-scoped by consumer policy.** §16.7 binds proxies — a
/// UAS answering an INVITE is governed by §8.2.6 — and under a paused test clock
/// a fixture may advance far past the grace in VIRTUAL time before answering,
/// which is no real-world latency at all.
pub struct Proxy100WithinGrace;

impl Obligation for Proxy100WithinGrace {
    fn id(&self) -> RuleId {
        RuleId::Proxy100WithinGrace
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per (taker, call, branch): the INVITE it took, whether it said it was
        // trying, and its earliest final on the transaction.
        let mut taken: BTreeMap<(&str, &str, &str), Taken<'_>> = BTreeMap::new();
        let mut said_trying: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        let mut first_final: BTreeMap<(&str, &str, &str), u64> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                continue;
            }
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    taken.entry((msg.dst.as_str(), msg.call_id.as_str(), branch)).or_insert(
                        Taken {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            cseq: msg.cseq,
                            sender: msg.src.as_str(),
                        },
                    );
                }
                Kind::Response { status } => {
                    let key = (msg.src.as_str(), msg.call_id.as_str(), branch);
                    if *status == 100 {
                        said_trying.insert(key);
                    } else if *status >= 200 {
                        let at = first_final.entry(key).or_insert(msg.at_us);
                        *at = (*at).min(msg.at_us);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (key, invite) in &taken {
            let finding = |decision| Finding {
                rule: RuleId::Proxy100WithinGrace,
                emitter: key.0.to_string(),
                taker: invite.sender.to_string(),
                cseq: invite.cseq,
                relayed: false,
                anchor: invite.msg,
                decision,
            };
            if said_trying.contains(key) {
                out.push(finding(Decision::Compliant));
                continue;
            }
            let after = first_final.get(key).map(|at| at.saturating_sub(invite.ts_us));
            match after {
                // §16.7 wants the 100 only where the answer is slow.
                Some(delta) if delta <= PROXY_100_GRACE_US => {
                    out.push(finding(Decision::Compliant))
                }
                // Nothing at all went out, and the observation may simply have
                // stopped before it could.
                None if !wire.obs.absence_decidable(invite.ts_us, PROXY_100_GRACE_US) => {
                    out.push(finding(Decision::Undecidable(
                        "the observation stopped inside the grace — truncation, not silence",
                    )))
                }
                _ => out.push(finding(Decision::Violated(Evidence::TryingNotSentInGrace {
                    trying_owed_msg: invite.msg,
                    trying_owed_hop: invite.hop,
                    trying_owed_ts_us: invite.ts_us,
                    branch: key.2.to_string(),
                    first_final_after_us: after,
                    grace_us: PROXY_100_GRACE_US,
                }))),
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The INVITE one endpoint took on one transaction.
#[derive(Debug)]
struct Taken<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    /// The party that sent it — the one owed the 100.
    sender: &'a str,
}

/// **RFC 3261 §16.3 — a proxy with no resolvable target answers 404.** When
/// target resolution yields nothing, "Not Found" is the answer that names the
/// outcome; any other error final tells the UAC the request failed for a reason
/// that did not happen, and a retry or a fallback is chosen on that lie.
///
/// The occasion is ONE request the endpoint took, never forwarded, and answered
/// with a 4xx/5xx/6xx final of its own. Charges that endpoint; a 404 discharges
/// it. Nothing else is an occasion: a forwarded request had a target, a
/// non-error final is not a resolution failure, and a request answered nothing
/// at all states no outcome to judge.
///
/// **"Forwarded" is correlated by Call-ID + CSeq, never by branch.** A §16.6
/// proxy mints a FRESH branch on the leg it forwards, so branch equality
/// systematically misses the forward and would charge every relaying proxy.
///
/// **A relayed rejection is not a no-target outcome**: where the endpoint TOOK
/// that same final status on the same Call-ID + CSeq, it did resolve a target
/// and the target said no — even when the forwarded leg itself rode a different
/// vantage.
///
/// A B2BUA worker legitimately rejects 403/481/491 without forwarding when the
/// backend refuses the call, which is why the live consumer takes this rule as
/// advisory and scopes it to declared proxies.
pub struct NoTarget404;

impl Obligation for NoTarget404 {
    fn id(&self) -> RuleId {
        RuleId::NoTarget404
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per endpoint: the transactions it forwarded, the finals it took, the
        // request it took first on each server transaction, and the first final
        // it answered there.
        let mut forwarded: BTreeSet<(&str, &str, u32, String)> = BTreeSet::new();
        let mut took_final: BTreeSet<(&str, &str, u32, String, u16)> = BTreeSet::new();
        let mut taken: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut answered: BTreeMap<(&str, &str, &str), u16> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let call = msg.call_id.as_str();
            let txn_method = msg.cseq_method.to_ascii_uppercase();
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { .. } => {
                    forwarded.insert((msg.src.as_str(), call, msg.cseq, txn_method));
                    if let Some(branch) = branch {
                        taken.entry((msg.dst.as_str(), call, branch)).or_insert(mi);
                    }
                }
                Kind::Response { status } if *status >= 200 => {
                    took_final.insert((msg.dst.as_str(), call, msg.cseq, txn_method, *status));
                    if let Some(branch) = branch {
                        answered.entry((msg.src.as_str(), call, branch)).or_insert(*status);
                    }
                }
                Kind::Response { .. } => {}
            }
        }

        let mut out = Vec::new();
        for ((endpoint, call, branch), mi) in &taken {
            let msg = &wire.msgs[*mi];
            let txn_method = msg.cseq_method.to_ascii_uppercase();
            if forwarded.contains(&(endpoint, call, msg.cseq, txn_method.clone())) {
                continue;
            }
            let Some(&status) = answered.get(&(endpoint, call, branch)) else { continue };
            if !(400..700).contains(&status) {
                continue;
            }
            if took_final.contains(&(endpoint, call, msg.cseq, txn_method, status)) {
                continue; // the downstream's rejection, relayed
            }
            let Kind::Request { method } = &msg.kind else { continue };
            out.push(Finding {
                rule: RuleId::NoTarget404,
                emitter: endpoint.to_string(),
                taker: msg.src.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: *mi,
                decision: if status == 404 {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::NoTargetFinal {
                        no_target_msg: *mi,
                        no_target_hop: msg.hop,
                        no_target_ts_us: msg.at_us,
                        method: method.to_string(),
                        branch: branch.to_string(),
                        status,
                    })
                },
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §16.4 — a strict-routed request is rewritten before it is
/// forwarded.** A topmost Route without `;lr` is a pre-loose-routing hop that
/// expects to be addressed in the Request-URI: the proxy moves that URI into
/// the request line (and its own former Request-URI onto the tail of the Route
/// set) before forwarding. Forwarded verbatim, the request reaches a hop that
/// is not the one the URI named.
///
/// **The occasion is one request an endpoint TOOK whose FIRST Route is
/// strict.** Nothing else is: a loose first route is the §16.12 path this rule
/// says nothing about, and a request with no Route set states no path to
/// rewrite. A vantage carrying no header block for the request cannot say
/// whether it was strict-routed at all, so it opens no occasion either —
/// unreadable bytes here withhold the OCCASION, where elsewhere in this crate
/// they withhold only the verdict.
///
/// **Discharge is the emitter's own forward of that transaction**: a request
/// of the same method it sent on the same branch, carrying the strict Route URI
/// as its Request-URI. Correlating on the branch the INCOMING request named is
/// what pairs a verbatim forward with what it should have rewritten; a proxy
/// that mints a fresh branch (§16.6) forwards a request this rule cannot pair,
/// and a §16.4 rewrite is judged against the copy that stayed on the branch.
/// Having forwarded NOTHING on it is the violation the tripwire is for.
///
/// Charges the endpoint that TOOK the strict-routed request — §16.4 is proxy
/// behaviour, and a UA that receives one forwards nothing at all, which is why
/// the live consumer scopes this to declared proxies.
pub struct StrictRouteRewriteHandled;

impl Obligation for StrictRouteRewriteHandled {
    fn id(&self) -> RuleId {
        RuleId::StrictRouteRewriteHandled
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            // The header block decides whether this is an occasion at all.
            let Some(head) = msg.head.as_deref() else { continue };
            // A row no reader accepts belongs to the grammar rules, not here.
            let Some(routes) = sniff::route_uris(head, "route") else { continue };
            let Some(first_route) = routes.first().filter(|r| !r.loose) else { continue };
            // The proxy is the endpoint the request arrived AT: it owes the
            // rewritten forward.
            let proxy = msg.dst.as_str();
            // The occasion rests on the request the proxy TOOK: that is where
            // the rewrite was owed.
            let finding = |decision| Finding {
                rule: RuleId::StrictRouteRewriteHandled,
                emitter: proxy.to_string(),
                taker: msg.src.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            };
            let forwarded = seen
                .at(&BranchKey { emitter: proxy, call_id: msg.call_id.as_str(), branch })
                .and_then(|b| b.first_sent(method));
            let Some(forwarded) = forwarded else {
                out.push(finding(Decision::Violated(Evidence::StrictRouteNotRewritten {
                    strict_route_msg: mi,
                    strict_route_hop: msg.hop,
                    strict_route_ts_us: msg.at_us,
                    method: method.to_string(),
                    branch: branch.to_string(),
                    first_route: first_route.uri.clone(),
                    forwarded_request_uri: String::new(),
                })));
                continue;
            };
            let sent_uri =
                wire.msgs[forwarded.msg].head.as_deref().and_then(sniff::request_uri_facts);
            let Some(sent_uri) = sent_uri else {
                out.push(finding(Decision::Undecidable(
                    "the forward's Request-URI is unreadable at this vantage",
                )));
                continue;
            };
            if sent_uri.uri == first_route.uri {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::StrictRouteNotRewritten {
                strict_route_msg: mi,
                strict_route_hop: msg.hop,
                strict_route_ts_us: msg.at_us,
                method: method.to_string(),
                branch: branch.to_string(),
                first_route: first_route.uri.clone(),
                forwarded_request_uri: sent_uri.uri,
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One INVITE transaction as the endpoint that OPENED it saw it. Spelled out as
/// a struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    /// The endpoint that sent the INVITEs and took the 100s.
    taker: &'a str,
    call_id: &'a str,
    cseq: u32,
}

/// The 100 that landed once the budget was already spent.
#[derive(Debug, Clone, Copy)]
struct Excess<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    src: &'a str,
}

/// One transaction's counts, as they stood over the whole view.
#[derive(Debug, Default)]
struct Txn<'a> {
    /// INVITEs the endpoint sent — each owed one replayed 100.
    sent: u32,
    /// 100s it took.
    taken: u32,
    /// Index of the FIRST INVITE, so a met obligation still anchors on the wire.
    first_invite: Option<usize>,
    /// Whom the INVITEs went to — the compliant finding's other party.
    peer: &'a str,
    /// The first 100 that arrived once `taken` had passed `sent`.
    excess: Option<Excess<'a>>,
}

/// What one view's messages say about the 100 budget on each transaction.
#[derive(Debug, Default)]
struct Reading<'a> {
    txns: BTreeMap<TxnKey<'a>, Txn<'a>>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// Absorb one message. Repeats are NOT folded away: a retransmitted INVITE
    /// is owed its own replayed 100, so both sides of the budget count every
    /// copy the wire carried.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        if msg.is_request("INVITE") {
            let txn = self
                .txns
                .entry(TxnKey {
                    taker: msg.src.as_str(),
                    call_id: msg.call_id.as_str(),
                    cseq: msg.cseq,
                })
                .or_default();
            txn.sent += 1;
            txn.first_invite.get_or_insert(mi);
            txn.peer = msg.dst.as_str();
            return;
        }
        if msg.status() != Some(100) || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
            return;
        }
        let txn = self
            .txns
            .entry(TxnKey {
                taker: msg.dst.as_str(),
                call_id: msg.call_id.as_str(),
                cseq: msg.cseq,
            })
            .or_default();
        txn.taken += 1;
        if txn.taken > txn.sent {
            txn.excess.get_or_insert(Excess {
                msg: mi,
                hop: msg.hop,
                ts_us: msg.at_us,
                src: msg.src.as_str(),
            });
        }
    }
}

/// **§16.6 step 6 — the strict-route swap runs BEFORE the request goes out.**
/// Where the route set's first URI carries no `;lr` it is a strict route, and
/// step 6.b has the forwarding element push the current Request-URI to the
/// bottom of the Route list and lift that first Route URI into the
/// Request-URI. A request already on the wire cannot replay the pre-swap state,
/// so what is read is the structural indicator the swap leaves behind: after
/// it, the topmost Route is no longer the strict hop.
///
/// The occasion is one fresh request the endpoint SENT that states a Route set
/// at all — a request with none is not routed through anything. A topmost
/// strict route on it either means the swap never ran, or that the next hop is
/// itself a strict-route target that survived it; both are worth surfacing, and
/// the sender is the party that would have run the swap.
///
/// A Route row no reader accepts opens NO occasion: nothing can say what the
/// route set was, and the grammar rules own the malformed row.
pub struct StrictRouteShuffleOnSend;

impl Obligation for StrictRouteShuffleOnSend {
    fn id(&self) -> RuleId {
        RuleId::StrictRouteShuffleOnSend
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            let Some(head) = msg.head.as_deref() else { continue };
            let Some(routes) = sniff::route_uris(head, "Route") else { continue };
            let Some(first) = routes.first() else { continue };
            let decision = if first.loose {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::HeaderValueRejected {
                    rejected_msg: mi,
                    rejected_hop: msg.hop,
                    rejected_ts_us: msg.at_us,
                    on: method.clone(),
                    header: "Route".to_string(),
                    value: first.uri.clone(),
                    expected: "a loose route (';lr') — the §16.6 step 6 swap lifts a strict one \
                               into the Request-URI"
                        .to_string(),
                })
            };
            out.push(Finding {
                rule: RuleId::StrictRouteShuffleOnSend,
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
mod tests {
    //! The rule's OWN semantics under a CLOSED observation: what the budget is,
    //! what a retransmission does to it, and what the wire cannot place.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        NoTarget404, Proxy100TryingNotForwarded, Proxy100WithinGrace, StrictRouteRewriteHandled,
        StrictRouteShuffleOnSend, PROXY_100_GRACE_US,
    };

    const ALICE: &str = "127.0.0.1:5060";
    const BOB: &str = "127.0.0.1:5070";

    fn invite(at_us: u64, cseq: u32) -> Msg {
        Msg {
            at_us,
            src: ALICE.to_string(),
            dst: BOB.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: "INVITE".to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: None,
            via_branch: Some("z9hG4bK-i".to_string()),
            head: None,
            body: None,
        }
    }

    fn trying(at_us: u64, cseq: u32) -> Msg {
        Msg {
            at_us,
            src: BOB.to_string(),
            dst: ALICE.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status: 100 },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: None,
            via_branch: Some("z9hG4bK-i".to_string()),
            head: None,
            body: None,
        }
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

    fn eval(msgs: &[Msg]) -> Vec<Finding> {
        Proxy100TryingNotForwarded.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn hits(msgs: &[Msg]) -> Vec<Finding> {
        eval(msgs).into_iter().filter(Finding::violated).collect()
    }

    /// One INVITE, one 100: the budget is met, and it is one occasion.
    #[test]
    fn one_trying_per_invite_is_compliant() {
        let msgs = [invite(1_000, 1), trying(2_000, 1)];
        let all = eval(&msgs);
        assert_eq!(all.len(), 1, "one occasion, the transaction: {all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
        assert_eq!(all[0].taker, ALICE, "read at the endpoint that opened the transaction");
    }

    /// A second 100 against one INVITE is a downstream copy the proxy should
    /// have absorbed, and it charges the hop that forwarded it.
    #[test]
    fn a_trying_in_excess_of_the_invites_sent_is_violated() {
        let msgs = [invite(1_000, 1), trying(2_000, 1), trying(3_000, 1)];
        let f = hits(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, BOB, "the hop that forwarded it is charged");
        assert_eq!(f[0].taker, ALICE);
        assert_eq!(f[0].anchor, 2, "the occasion rests on the first excess 100");
        let Decision::Violated(Evidence::ExtraTryingForwarded {
            trying_taken, invites_sent, ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*trying_taken, *invites_sent), (2, 1));
    }

    /// A retransmitted INVITE is owed the replayed 100 (§17.2.1): the budget
    /// grows with the ladder, and only the copy past it is forwarded.
    #[test]
    fn a_replayed_trying_per_retransmitted_invite_is_owed() {
        let ladder = [
            invite(1_000, 1),
            trying(2_000, 1),
            invite(3_000, 1),
            trying(4_000, 1),
            invite(5_000, 1),
            trying(6_000, 1),
        ];
        assert!(hits(&ladder).is_empty(), "one per INVITE sent: {:?}", hits(&ladder));

        let mut extra = ladder.to_vec();
        extra.push(trying(7_000, 1));
        let f = hits(&extra);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::ExtraTryingForwarded {
            trying_taken, invites_sent, ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*trying_taken, *invites_sent), (4, 3));
    }

    /// Three stray copies are ONE defect on ONE transaction, and the count is
    /// on the finding.
    #[test]
    fn several_stray_copies_are_one_occasion() {
        let msgs = [invite(1_000, 1), trying(2_000, 1), trying(3_000, 1), trying(4_000, 1)];
        let f = hits(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].anchor, 2, "anchored where the count stopped being owed");
        let Decision::Violated(Evidence::ExtraTryingForwarded { trying_taken, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*trying_taken, 3, "every copy the taker took is counted");
    }

    /// A 100 on a transaction this vantage never saw opened opens no occasion.
    #[test]
    fn a_trying_for_an_unseen_invite_opens_no_occasion() {
        assert!(eval(&[trying(1_000, 9)]).is_empty());
    }

    /// The transactions are keyed per call: a second call's 100 is not this
    /// call's excess.
    #[test]
    fn separate_calls_do_not_alias() {
        let other = |mut m: Msg| {
            m.call_id = "c2".to_string();
            m
        };
        let msgs =
            [invite(1_000, 1), trying(2_000, 1), other(invite(3_000, 1)), other(trying(4_000, 1))];
        assert!(hits(&msgs).is_empty(), "{:?}", hits(&msgs));
    }

    // ── NoTarget404 (RFC 3261 §16.3) ────────────────────────────────────────

    const PROXY: &str = "127.0.0.1:5080";

    /// A request into the proxy, or out of it, on a caller-chosen branch.
    fn hop_request(at_us: u64, src: &str, dst: &str, branch: &str, cseq: u32) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: "INVITE".to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: None,
            via_branch: Some(branch.to_string()),
            head: None,
            body: None,
        }
    }

    fn hop_response(at_us: u64, src: &str, dst: &str, status: u16, branch: &str, cseq: u32) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: Some("bt".to_string()),
            via_branch: Some(branch.to_string()),
            head: None,
            body: None,
        }
    }

    /// The rule's findings charged to `endpoint` — what the live consumer keeps
    /// (violated, at the vantage bind). Any endpoint that takes a request and
    /// answers it without forwarding is judged here; scoping the rule to
    /// DECLARED proxies is the consumer's `{Proxy}` subject, not the body's.
    fn charged_to(endpoint: &str, msgs: &[Msg]) -> Vec<Finding> {
        NoTarget404
            .eval(&WireView { msgs, obs: &obs(msgs) })
            .into_iter()
            .filter(|f| f.emitter == endpoint)
            .collect()
    }

    /// 404 is the answer §16.3 names, so the occasion is met.
    #[test]
    fn a_404_without_forwarding_is_compliant() {
        let msgs = [
            hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
            hop_response(2_000, PROXY, ALICE, 404, "z9hG4bK-in", 1),
        ];
        let f = charged_to(PROXY, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, PROXY, "the proxy that resolved nothing is charged");
        assert_eq!(f[0].taker, ALICE);
    }

    /// Any other error final names an outcome that did not happen.
    #[test]
    fn another_error_final_without_forwarding_is_violated() {
        let msgs = [
            hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
            hop_response(2_000, PROXY, ALICE, 500, "z9hG4bK-in", 1),
        ];
        let f = charged_to(PROXY, &msgs).into_iter().filter(Finding::violated).collect::<Vec<_>>();
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::NoTargetFinal { status, method, branch, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*status, method.as_str(), branch.as_str()), (500, "INVITE", "z9hG4bK-in"));
    }

    /// A §16.6 proxy mints a FRESH branch on the leg it forwards, so the
    /// forward is correlated by Call-ID + CSeq — never by branch.
    #[test]
    fn a_forward_on_a_fresh_branch_resolves_a_target() {
        let msgs = [
            hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
            hop_request(2_000, PROXY, BOB, "z9hG4bK-out", 1),
            hop_response(3_000, BOB, PROXY, 486, "z9hG4bK-out", 1),
            hop_response(4_000, PROXY, ALICE, 486, "z9hG4bK-in", 1),
        ];
        assert!(charged_to(PROXY, &msgs).is_empty(), "{:?}", charged_to(PROXY, &msgs));
        assert_eq!(
            charged_to(BOB, &msgs).len(),
            1,
            "the far UAS did reject without forwarding — the consumer's {{Proxy}} \
             subject is what keeps a UA off this rule, never the body"
        );
    }

    /// Even where the forwarded leg rode another vantage, a final that MATCHES
    /// one the endpoint took on the same transaction is a relayed rejection.
    #[test]
    fn a_relayed_downstream_rejection_is_not_a_no_target_outcome() {
        let msgs = [
            hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
            hop_response(2_000, BOB, PROXY, 486, "z9hG4bK-other", 1),
            hop_response(3_000, PROXY, ALICE, 486, "z9hG4bK-in", 1),
        ];
        assert!(charged_to(PROXY, &msgs).is_empty(), "{:?}", charged_to(PROXY, &msgs));
    }

    /// A non-error final states no resolution failure, and an unanswered
    /// request states no outcome at all.
    #[test]
    fn only_an_error_final_opens_the_occasion() {
        let served = [
            hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1),
            hop_response(2_000, PROXY, ALICE, 200, "z9hG4bK-in", 1),
        ];
        assert!(charged_to(PROXY, &served).is_empty());
        let silent = [hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1)];
        assert!(charged_to(PROXY, &silent).is_empty());
    }

    // ── StrictRouteRewriteHandled (RFC 3261 §16.4) ──────────────────────────

    /// A request crossing a hop on `branch`, with a caller-chosen Request-URI
    /// and Route row in the head the §16.4 rule reads.
    fn routed(
        at_us: u64,
        src: &str,
        dst: &str,
        branch: &str,
        request_uri: &str,
        route: &str,
    ) -> Msg {
        let head = format!(
            "INVITE {request_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             {route}\r\n"
        );
        let mut m = hop_request(at_us, src, dst, branch, 1);
        m.head = Some(head.into_bytes());
        m
    }

    /// The rule's findings charged to `endpoint`. Every hop that TAKES a
    /// strict-routed request owes the rewrite, so a two-hop trace judges both;
    /// scoping to DECLARED proxies is the consumer's `{Proxy}` subject, never
    /// the body's.
    fn strict_at(endpoint: &str, msgs: &[Msg]) -> Vec<Finding> {
        StrictRouteRewriteHandled
            .eval(&WireView { msgs, obs: &obs(msgs) })
            .into_iter()
            .filter(|f| f.emitter == endpoint)
            .collect()
    }

    fn strict(msgs: &[Msg]) -> Vec<Finding> {
        strict_at(PROXY, msgs)
    }

    const STRICT_ROUTE: &str = "Route: <sip:strict@h>\r\n";

    /// A LOOSE first route is the §16.12 path this rule says nothing about.
    #[test]
    fn a_loose_first_route_is_no_occasion() {
        let msgs =
            [routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", "Route: <sip:p@h;lr>\r\n")];
        assert!(strict(&msgs).is_empty(), "{:?}", strict(&msgs));
    }

    /// The tripwire: a strict-routed request taken and never forwarded at all.
    #[test]
    fn a_strict_route_never_forwarded_is_violated() {
        let msgs = [routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE)];
        let f = strict(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, PROXY, "the box that took it owes the rewrite");
        assert_eq!(f[0].taker, ALICE);
        let Decision::Violated(Evidence::StrictRouteNotRewritten {
            first_route,
            forwarded_request_uri,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("strict-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(first_route, "sip:strict@h");
        assert!(forwarded_request_uri.is_empty(), "nothing was forwarded on the branch");
        assert_eq!(branch, "z9hG4bK-in");
    }

    /// The §16.4 swap performed: the strict Route URI became the forwarded
    /// Request-URI.
    #[test]
    fn a_strict_route_swapped_into_the_request_uri_is_compliant() {
        let msgs = [
            routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
            routed(2_000, PROXY, BOB, "z9hG4bK-in", "sip:strict@h", "Route: <sip:bob@h>\r\n"),
        ];
        let f = strict(&msgs);
        assert_eq!(f.len(), 1, "one occasion, the request taken: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(
            strict_at(BOB, &msgs).len(),
            1,
            "the next hop took a strict-routed request too — its own occasion",
        );
    }

    /// Forwarded verbatim: the request reaches a hop the strict URI did not
    /// name, and the evidence carries both URIs.
    #[test]
    fn a_strict_route_forwarded_verbatim_is_violated() {
        let msgs = [
            routed(1_000, ALICE, PROXY, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
            routed(2_000, PROXY, BOB, "z9hG4bK-in", "sip:bob@h", STRICT_ROUTE),
        ];
        let f = strict(&msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::StrictRouteNotRewritten {
            first_route,
            forwarded_request_uri,
            ..
        }) = &f[0].decision
        else {
            panic!("strict-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(
            (first_route.as_str(), forwarded_request_uri.as_str()),
            ("sip:strict@h", "sip:bob@h")
        );
    }

    /// The header block is what says a request was strict-routed at all: a
    /// vantage that carried none opens NO occasion, rather than an undecidable
    /// one against every request on the wire.
    #[test]
    fn a_request_without_header_bytes_is_no_occasion() {
        let msgs = [hop_request(1_000, ALICE, PROXY, "z9hG4bK-in", 1)];
        assert!(strict(&msgs).is_empty(), "{:?}", strict(&msgs));
    }

    // ── Proxy100WithinGrace (RFC 3261 §16.7) ────────────────────────────────

    /// A final bob sent on the INVITE's own transaction.
    fn answered(at_us: u64, status: u16) -> Msg {
        Msg {
            at_us,
            src: BOB.to_string(),
            dst: ALICE.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: "INVITE".to_string(),
            from_tag: Some("at".to_string()),
            to_tag: Some("bt".to_string()),
            via_branch: Some("z9hG4bK-i".to_string()),
            head: None,
            body: None,
        }
    }

    fn grace(msgs: &[Msg]) -> Vec<Finding> {
        Proxy100WithinGrace.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// The 100 went out: the obligation is met, and the endpoint charged is the
    /// one that TOOK the INVITE.
    #[test]
    fn a_trying_on_the_transaction_is_compliant() {
        let f = grace(&[invite(1_000, 1), trying(2_000, 1)]);
        assert_eq!(f.len(), 1, "one occasion, the INVITE taken: {f:?}");
        assert_eq!(f[0].rule, RuleId::Proxy100WithinGrace);
        assert_eq!(f[0].emitter, BOB, "the hop that took the INVITE is charged");
        assert_eq!(f[0].taker, ALICE);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// §16.7 wants the 100 only where the answer is slow: a hop that rejected
    /// inside the grace owes nobody a Trying.
    #[test]
    fn a_final_inside_the_grace_discharges_it() {
        let f = grace(&[invite(1_000, 1), answered(1_000 + PROXY_100_GRACE_US, 486)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// Past the grace with no 100, the finding names the Δ actually observed.
    #[test]
    fn a_final_past_the_grace_is_violated_with_its_delta() {
        let late = 1_000 + PROXY_100_GRACE_US + 1;
        let f = grace(&[invite(1_000, 1), answered(late, 486)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::TryingNotSentInGrace {
            first_final_after_us,
            grace_us,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*first_final_after_us, Some(PROXY_100_GRACE_US + 1));
        assert_eq!(*grace_us, PROXY_100_GRACE_US);
        assert_eq!(branch.as_str(), "z9hG4bK-i");
    }

    /// Nothing went out at all — and a CLOSED observation says so.
    #[test]
    fn silence_on_a_closed_observation_is_violated() {
        let f = grace(&[invite(1_000, 1)]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::TryingNotSentInGrace { first_final_after_us, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*first_final_after_us, None, "no response was sent");
    }

    /// An OPEN observation that stopped inside the grace shows truncation, not
    /// a hop holding its tongue.
    #[test]
    fn silence_inside_the_grace_of_an_open_observation_is_undecidable() {
        let msgs = [invite(1_000, 1)];
        let open = Observation { last_us: 1_100, closed: false, ..Observation::default() };
        let f = Proxy100WithinGrace.eval(&WireView { msgs: &msgs, obs: &open });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);
    }

    /// A retransmitted INVITE is the same transaction: the 100 the hop already
    /// sent still discharges it, and it stays ONE occasion.
    #[test]
    fn a_retransmitted_invite_reuses_the_occasion() {
        let f = grace(&[invite(1_000, 1), trying(2_000, 1), invite(3_000, 1)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    // ---- strict-route-shuffle-on-send -----------------------------------

    /// A request the hop forwarded, stating `route` at the top of its Route set
    /// (`""` = no Route header at all).
    fn forwarded(route: &str) -> Msg {
        let row = if route.is_empty() { String::new() } else { format!("Route: {route}\r\n") };
        let head = format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-r\r\n\
             {row}\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = invite(1_000, 1);
        m.head = Some(head.into_bytes());
        m
    }

    fn shuffle(msgs: &[Msg]) -> Vec<Finding> {
        StrictRouteShuffleOnSend.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    #[test]
    fn a_loose_topmost_route_is_compliant() {
        let f = shuffle(&[forwarded("<sip:proxy@127.0.0.1;lr>")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_strict_topmost_route_is_violated() {
        let f = shuffle(&[forwarded("<sip:proxy@127.0.0.1>")]);
        let Decision::Violated(Evidence::HeaderValueRejected { header, value, expected, .. }) =
            &f[0].decision
        else {
            panic!("strict-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(header, "Route");
        assert!(value.contains("proxy@127.0.0.1"), "{value}");
        assert!(expected.contains(";lr"), "{expected}");
    }

    /// A request stating no route set is routed through nothing, and a vantage
    /// with no header block cannot say whether it stated one.
    #[test]
    fn a_routeless_or_byte_less_request_is_no_occasion() {
        assert!(shuffle(&[forwarded("")]).is_empty());
        assert!(shuffle(&[invite(1_000, 1)]).is_empty());
    }
}
