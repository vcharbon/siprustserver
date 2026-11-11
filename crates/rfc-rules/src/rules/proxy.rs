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
//!   - [`StrictRouteRewriteHandled`] (§16.4, §16.6 step 6) charges the proxy
//!     that took a strict-routed request and forwarded it without the
//!     Request-URI rewrite.
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

use std::collections::BTreeMap;

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::correlation::Parties;
use super::sides::Sides;
use super::Obligation;
use owed_request_uri::owed_request_uri;

mod grace;
mod no_target;
mod owed_request_uri;

pub use grace::{Proxy100WithinGrace, PROXY_100_GRACE_US};
pub use no_target::NoTarget404;

crate::registry::rules! {
    /// RFC 3261 §16.7 step 5: a stateful proxy absorbs the downstream 100 Trying, so a UAC takes at
    /// most one 100 per INVITE it sent on the transaction. Charges the endpoint that forwarded the
    /// extra 100.
    Proxy100TryingNotForwarded = "proxy-100-trying-not-forwarded";
    /// RFC 3261 §16.3: a proxy that resolves no target for the Request-URI answers 404. Charges the
    /// proxy.
    NoTarget404 = "no-target-404";
    /// RFC 3261 §16.4: a proxy forwarding a request whose topmost Route is strict swaps that URI
    /// into the Request-URI. Charges the proxy that took the request.
    StrictRouteRewriteHandled = "strict-route-rewrite-handled";
    /// RFC 3261 §16.7: a proxy that cannot answer an INVITE promptly emits 100 Trying within its
    /// grace window. Charges the endpoint that took the INVITE.
    Proxy100WithinGrace = "proxy-100-within-grace";
    /// RFC 3261 §16.6 step 6: a request forwarded through a strict-route next hop has had the
    /// Request-URI / Route swap applied, so its topmost Route is no longer a strict one. Charges
    /// the forwarding hop.
    StrictRouteShuffleOnSend = "strict-route-shuffle-on-send";
}

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

/// **RFC 3261 §16.4 / §16.6 step 6 — a strict-routed request is rewritten
/// before it is forwarded.** A Route without `;lr` is a pre-loose-routing hop
/// that expects to be addressed in the Request-URI. Two shapes reach a proxy:
///
/// - **addressed to it**: its Request-URI names the proxy (a peer
///   strict-routed to the URI the proxy record-routed, §12.2.1.1) under no
///   loose top Route naming the proxy, which is loose routing. §16.4 has
///   the proxy take the LAST Route as Request-URI; §16.6 step 6 then lifts the
///   next Route into it if that one is strict too.
/// - **a strict first Route**, once a loose top Route naming the proxy is
///   popped: the proxy moves that URI into the request line (and its own
///   former Request-URI onto the tail of the Route set).
///
/// Forwarded without the rewrite, the request reaches a hop that is not the
/// one the route set named.
///
/// **The occasion is one request an endpoint TOOK in one of those shapes.**
/// Nothing else is: a loose first route is the §16.12 path this rule says
/// nothing about, and a request with no Route set states no path to rewrite. A
/// vantage carrying no header block for the request cannot say whether it was
/// strict-routed at all, so it opens no occasion either — unreadable bytes
/// here withhold the OCCASION, where elsewhere in this crate they withhold only
/// the verdict.
///
/// **Discharge is the emitter's own forward of that transaction**: a request
/// of the same method it sent on the same call whose Via stack carries the
/// branch the INCOMING request named — on top when it stayed on the branch,
/// under the proxy's own Via when it forwarded statefully (§16.6 step 8) —
/// with the URI the rewrite owes as its Request-URI, sent from a socket of the
/// user agent that took it ([`Parties`]). Having forwarded NOTHING is the
/// violation the tripwire is for.
///
/// Charges the endpoint that TOOK the strict-routed request — §16.4 is proxy
/// behaviour, and a UA that receives one forwards nothing at all, which is why
/// the live consumer scopes this to declared proxies.
pub struct StrictRouteRewriteHandled;

impl Obligation for StrictRouteRewriteHandled {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let parties = Parties::of(wire.msgs);
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
            // The proxy is the endpoint the request arrived AT: it owes the
            // rewritten forward.
            let proxy = msg.dst.as_str();
            let Some(owed) = owed_request_uri(head, &routes, proxy) else { continue };
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
            let violated = |forwarded_request_uri: String| {
                Decision::Violated(Evidence::StrictRouteNotRewritten {
                    strict_route_msg: mi,
                    strict_route_hop: msg.hop,
                    strict_route_ts_us: msg.at_us,
                    method: method.to_string(),
                    branch: branch.to_string(),
                    first_route: owed.clone(),
                    forwarded_request_uri,
                })
            };
            let forwarded = wire.msgs.iter().skip(mi + 1).find(|m| {
                !m.repeat
                    && m.call_id == msg.call_id
                    && parties.same(&msg.call_id, &m.src, &msg.dst)
                    && m.is_request(method)
                    && m.head
                        .as_deref()
                        .is_some_and(|h| sniff::via_branches(h).iter().any(|b| b == branch))
            });
            let Some(forwarded) = forwarded else {
                out.push(finding(violated(String::new())));
                continue;
            };
            let sent_uri = forwarded.head.as_deref().and_then(sniff::request_uri_facts);
            let Some(sent_uri) = sent_uri else {
                out.push(finding(Decision::Undecidable(
                    "the forward's Request-URI is unreadable at this vantage",
                )));
                continue;
            };
            if sent_uri.uri == owed {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(violated(sent_uri.uri)));
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
        let sides = Sides::of(msgs);
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(&sides, mi, msg);
        }
        seen
    }

    /// Absorb one message. Repeats are NOT folded away: a retransmitted INVITE
    /// is owed its own replayed 100, so both sides of the budget count every
    /// copy the wire carried. A 100 is counted at the socket its INVITE left
    /// from ([`Sides`]), wherever the UAC's sent-by took it.
    fn absorb(&mut self, sides: &Sides<'a>, mi: usize, msg: &'a Msg) {
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
            .entry(TxnKey { taker: sides.uac(msg), call_id: msg.call_id.as_str(), cseq: msg.cseq })
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
/// Request-URI. After the swap the topmost Route may still lack `;lr` — the
/// Request-URI it pushed down is typically a Contact — so a strict topmost
/// Route is judged against the request the hop forwarded, when this vantage
/// carried it (the received top-Via branch in the sent Via stack): the swap
/// ran when the sent Request-URI is the one that request owed — the first
/// strict Route left once §16.4 processed it.
/// Otherwise the sender, the party that would have run the swap, is charged.
///
/// The occasion is one fresh request the endpoint SENT that states a Route set
/// at all — a request with none is not routed through anything.
///
/// A Route row no reader accepts opens NO occasion: nothing can say what the
/// route set was, and the grammar rules own the malformed row.
pub struct StrictRouteShuffleOnSend;

impl Obligation for StrictRouteShuffleOnSend {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let parties = Parties::of(wire.msgs);
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            let Some(head) = msg.head.as_deref() else { continue };
            let Some(routes) = sniff::route_uris(head, "Route") else { continue };
            let Some(first) = routes.first() else { continue };
            let decision = if first.loose || swapped(wire.msgs, &parties, mi, head) {
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

/// Whether the request at `mi` (header block `head`) is the §16.6 step 6 swap
/// of a request its sender took at a socket of its user agent ([`Parties`]):
/// that request rides below the sender's own Via, and the sent Request-URI is
/// the one [`owed_request_uri`] names for it.
fn swapped(msgs: &[Msg], parties: &Parties, mi: usize, head: &[u8]) -> bool {
    let sent = &msgs[mi];
    let Some(request_uri) = sniff::request_uri_facts(head) else { return false };
    let below = sniff::via_branches(head);
    let below = below.get(1..).unwrap_or_default();
    let taken = msgs[..mi].iter().rev().find(|taken| {
        !taken.repeat
            && taken.call_id == sent.call_id
            && parties.same(&sent.call_id, &taken.dst, &sent.src)
            && matches!((&taken.kind, &sent.kind),
                (Kind::Request { method: a }, Kind::Request { method: b }) if a.eq_ignore_ascii_case(b))
            && taken.via_branch.as_deref().is_some_and(|b| below.iter().any(|v| v == b))
    });
    let Some(taken) = taken else { return false };
    let Some(taken_head) = taken.head.as_deref() else { return false };
    let Some(routes) = sniff::route_uris(taken_head, "Route") else { return false };
    owed_request_uri(taken_head, &routes, taken.dst.as_str())
        .is_some_and(|owed| owed == request_uri.uri)
}

#[cfg(test)]
mod tests;
