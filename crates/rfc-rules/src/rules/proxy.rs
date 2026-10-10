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

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

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
/// with the URI the rewrite owes as its Request-URI. Having forwarded NOTHING
/// is the violation the tripwire is for.
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
                    && m.src == msg.dst
                    && m.call_id == msg.call_id
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

/// The Request-URI a proxy at `proxy` owes the forward of a request carrying
/// `head` and `routes`, or `None` where the request is not strict-routed.
///
/// - Addressed to the proxy (§16.4): its Request-URI names the proxy — a URI
///   the proxy record-routed, `;lr` or not, since a strict-routing peer
///   addresses whatever URI it was given — no loose top Route names the
///   proxy, and the last Route is not loose: a strict-routed request carries
///   the remote target last (§12.2.1.1). That target is owed; a strict Route
///   left first takes its place (§16.6 step 6).
/// - Otherwise a loose top Route naming the proxy is popped (§16.4), and the
///   first Route left is owed the Request-URI when it is strict (§16.6 step 6).
fn owed_request_uri(head: &[u8], routes: &[sniff::UriFacts], proxy: &str) -> Option<String> {
    let at_proxy = |host: &str, port: u16| {
        super::dialog::wire_addr(proxy).is_some_and(|(h, p)| h == host && p == port)
    };
    let loose_to_proxy = routes.first().is_some_and(|r| r.loose && at_proxy(&r.host, r.port));
    let target_last = routes.last().is_some_and(|r| !r.loose);
    let addressed = !loose_to_proxy
        && target_last
        && sniff::request_uri_facts(head).is_some_and(|uri| at_proxy(&uri.host, uri.port));
    if addressed {
        let (last, rest) = routes.split_last()?;
        return Some(match rest.first() {
            Some(next) if !next.loose => next.uri.clone(),
            _ => last.uri.clone(),
        });
    }
    let left = if loose_to_proxy { &routes[1..] } else { routes };
    left.first().filter(|r| !r.loose).map(|r| r.uri.clone())
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
/// Request-URI. After the swap the topmost Route may still lack `;lr` — the
/// Request-URI it pushed down is typically a Contact — so a strict topmost
/// Route is judged against the request the hop forwarded, when this vantage
/// carried it (the received top-Via branch in the sent Via stack): the swap
/// ran when the sent Request-URI is the one that request owed — the first
/// strict Route left once §16.4 processed it.
/// Otherwise a topmost strict route either means the swap never ran, or that
/// the next hop is itself a strict-route target that survived it; both are
/// worth surfacing, and the sender is the party that would have run the swap.
///
/// The occasion is one fresh request the endpoint SENT that states a Route set
/// at all — a request with none is not routed through anything.
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
            let decision = if first.loose || swapped(wire.msgs, mi, head) {
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
/// of a request its sender took: that request rides below the sender's own
/// Via, and the sent Request-URI is the one [`owed_request_uri`] names for it.
fn swapped(msgs: &[Msg], mi: usize, head: &[u8]) -> bool {
    let sent = &msgs[mi];
    let Some(request_uri) = sniff::request_uri_facts(head) else { return false };
    let below = sniff::via_branches(head);
    let below = below.get(1..).unwrap_or_default();
    let taken = msgs[..mi].iter().rev().find(|taken| {
        !taken.repeat
            && taken.dst == sent.src
            && taken.call_id == sent.call_id
            && matches!((&taken.kind, &sent.kind),
                (Kind::Request { method: a }, Kind::Request { method: b }) if a.eq_ignore_ascii_case(b))
            && taken.via_branch.as_deref().is_some_and(|b| below.iter().any(|v| v == b))
    });
    let Some(taken_head) = taken.and_then(|t| t.head.as_deref()) else { return false };
    let Some(routes) = sniff::route_uris(taken_head, "Route") else { return false };
    owed_request_uri(taken_head, &routes, sent.src.as_str())
        .is_some_and(|owed| owed == request_uri.uri)
}

#[cfg(test)]
mod tests;
