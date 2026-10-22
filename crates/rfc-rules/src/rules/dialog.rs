//! RFC 3261 §12 — what establishing a dialog FIXES, what every later message on
//! it must reproduce, and (§15) what may end it. SIX obligations, each read
//! against one endpoint's own view of one dialog:
//!
//!   - [`MidDialogUri`] (§12.2.1.1) charges the UAC: an in-dialog request
//!     repeats the peer URIs the dialog was created with.
//!   - [`MidDialogRoute`] (§12.2.1.1 / §16.12) charges the UAC: an in-dialog
//!     request reproduces the dialog route set — as Route rows behind a loose
//!     first route, in the Request-URI behind a strict one.
//!   - [`MidDialogWireDestination`] (§8.1.2 + RFC 3263 §4) charges the UAC: the
//!     bytes go to the destination the request's own routing names.
//!   - [`RecordRoutePlacement`] (§12.1.1 / §12.2.2) charges the responder:
//!     Record-Route rides dialog-CREATING responses only, so a 100 must not
//!     carry one and neither must a response to an in-dialog request.
//!   - [`UnknownDialog481`] (§12.2.2) charges the TAKER: an in-dialog request
//!     naming a dialog it never confirmed is answered 481, not served.
//!   - [`NoByeOutsideOrEarlyDialog`] (§15) charges the BYE's sender: a BYE
//!     names a dialog that exists, and the callee of one it has not accepted
//!     rejects or CANCELs rather than BYE-ing.
//!
//! **A dialog is `(endpoint, Call-ID, PEER tag)`.** The peer's tag is the half
//! of the §12 dialog id this endpoint does not mint, and it is direction-
//! independent: a request the endpoint SENT names its peer in To, a request it
//! TOOK names its peer in From, and the two orientations of one dialog collapse
//! onto one key. That is what keeps a callee-initiated re-INVITE or BYE — whose
//! From/To are reversed relative to the establishing INVITE — on the dialog it
//! actually rides.
//!
//! **Before the peer has a tag there is one PENDING state per
//! `(endpoint, Call-ID)`**, and every tag that later appears is seeded from it.
//! The UAC learns its peer's tag only from the first tagged response, so the
//! establishing INVITE — which fixes the dialog URIs and the initial-INVITE
//! branch — is recorded there and copied into each dialog that materialises.
//! Forking (§12.1.2) mints several peer tags off ONE INVITE, and each fork must
//! carry that INVITE's URIs and learn its OWN route set from its OWN
//! dialog-creating response. At the UAS there is no pending phase: the caller's
//! From tag is on the establishing INVITE itself.
//!
//! **Only the dialog-ESTABLISHING INVITE (no To tag) seeds role and URIs.** A
//! UAS-initiated re-INVITE carries both tags in the reverse orientation and
//! shares the dialog with the establishing INVITE; taking it for the initial one
//! would flip the endpoint's role and corrupt the route set it learned.
//!
//! **Facts beyond the wire model come from `Msg::head` via `sip_message::sniff`
//! — never parsed here.** A message the vantage carried no header block for
//! settles nothing about the URIs or Routes it stated: `Undecidable`, never
//! clean. So is a Route row no reader accepts — the routing rules judge the
//! path a peer EXPRESSED, and an unreadable row is the grammar rules' finding.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff::{self, UriFacts};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;
use message_facts::{record_route_set, remote_tag_of, request_uri_target, set_if_empty};

mod message_facts;

crate::registry::rules! {
    /// RFC 3261 §12.2.1.1: an in-dialog request repeats the dialog's peer URIs — the From/To URIs
    /// learned when the dialog was created. Charges the UAC that sent it.
    MidDialogUri = "mid-dialog-uri";
    /// RFC 3261 §12.2.1.1 / §16.12: an in-dialog request reproduces the dialog route set — as Route
    /// rows behind a loose first route, or in the Request-URI behind a strict one. Charges the UAC
    /// that sent it.
    MidDialogRoute = "mid-dialog-route";
    /// RFC 3261 §8.1.2 + RFC 3263 §4: an in-dialog request's bytes go to the destination its
    /// topmost Route (or, with an empty route set, its Request-URI) resolves to. Charges the UAC
    /// that sent it.
    MidDialogWireDestination = "mid-dialog-wire-destination";
    /// RFC 3261 §12.1.1 / §12.2.2: Record-Route rides dialog-CREATING responses only — never a 100,
    /// never a response to an in-dialog request. Charges the endpoint that sent the response.
    RecordRoutePlacement = "record-route-placement";
    /// RFC 3261 §12.2.2: a UAS that takes an in-dialog request naming a dialog it never confirmed
    /// answers 481. Charges that UAS.
    UnknownDialog481 = "unknown-dialog-481";
    /// RFC 3261 §15: a BYE names a dialog that exists, and a callee does not BYE an early one — it
    /// CANCELs or answers 4xx/5xx/6xx. Charges the BYE's sender.
    NoByeOutsideOrEarlyDialog = "no-bye-outside-or-early-dialog";
}

/// **§12.2.1.1 — an in-dialog request repeats the dialog's peer URIs.** The
/// From and To URIs of every in-dialog request a UAC sends are the local and
/// remote URIs the dialog was created with; rewriting either mid-dialog breaks
/// the peer's dialog matching, and a real UAS answers 481.
///
/// The occasion is ONE in-dialog request the endpoint sent on a dialog whose
/// creation this vantage carried. Charges the endpoint that sent it. A dialog
/// this vantage never saw created settles nothing — `Undecidable`, since the
/// URIs it owed are unknown. Both URIs are judged on ONE finding: they are one
/// act of rewriting the dialog's identity, not two.
///
/// This is the teeth for a B2BUA that rewrites From or To on a re-INVITE,
/// UPDATE or BYE. The test UA answers it regardless, so nothing else catches it.
pub struct MidDialogUri;

impl Obligation for MidDialogUri {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for req in &seen.requests {
            // CANCEL is hop-by-hop (§9.1) and the dialog-creating INVITE
            // precedes the dialog: neither is an in-dialog request.
            if req.method.eq_ignore_ascii_case("CANCEL") || req.initial_invite || !req.in_dialog {
                continue;
            }
            let finding = |decision| req.finding(RuleId::MidDialogUri, decision);
            if req.dialog.local_uri.is_empty() && req.dialog.remote_uri.is_empty() {
                out.push(finding(Decision::Undecidable(
                    "no dialog-creating INVITE at this vantage",
                )));
                continue;
            }
            let Some(head) = wire.msgs[req.msg].head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let (Some(sent_from), Some(sent_to)) =
                (sniff::name_addr_uri(head, "From"), sniff::name_addr_uri(head, "To"))
            else {
                out.push(finding(Decision::Undecidable("no readable From/To at this vantage")));
                continue;
            };
            let from_bad = !req.dialog.local_uri.is_empty() && sent_from != req.dialog.local_uri;
            let to_bad = !req.dialog.remote_uri.is_empty() && sent_to != req.dialog.remote_uri;
            if !from_bad && !to_bad {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::MidDialogUriChanged {
                uri_msg: req.msg,
                uri_hop: req.hop,
                uri_ts_us: req.ts_us,
                method: req.method.to_string(),
                sent_from_uri: sent_from,
                dialog_local_uri: req.dialog.local_uri.clone(),
                sent_to_uri: sent_to,
                dialog_remote_uri: req.dialog.remote_uri.clone(),
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **§12.2.1.1 / §16.12 — an in-dialog request reproduces the dialog route
/// set.** The route set is fixed at establishment (the Record-Route stack,
/// reversed for the UAC). With a LOOSE first route the request carries the set
/// verbatim as Route rows; with a STRICT first route the Request-URI becomes
/// the first route URI and the rest ride the Route rows, optionally with the
/// target appended. Dropping or reordering it mis-routes the request past the
/// proxies that record-routed.
///
/// The occasion is ONE in-dialog request the endpoint sent on a dialog with a
/// NON-EMPTY route set — an empty set asks for nothing. Charges the sender.
/// ACK and CANCEL are §17/§9.1 hop-by-hop and not governed by the set.
///
/// **The loose comparison weighs the routing-significant `host:port`, not the
/// whole URI.** A record-routing proxy legitimately rewrites Record-Route URI
/// PARAMETERS per direction (a signed stateful cookie toward one leg, a bare
/// `;outbound` toward the other), so demanding verbatim parameter equality
/// would false-fire on every stateful proxy. What §12.2.1.1 asks — the set
/// reproduced in order to the same hops, with none dropped, reordered or
/// re-aimed — is exactly what `host:port` in sequence captures.
pub struct MidDialogRoute;

impl Obligation for MidDialogRoute {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for req in &seen.requests {
            if req.hop_by_hop() || req.initial_invite || !req.in_dialog {
                continue;
            }
            if req.dialog.route_set.is_empty() {
                continue;
            }
            let finding = |decision| req.finding(RuleId::MidDialogRoute, decision);
            let Some(head) = wire.msgs[req.msg].head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let Some(sent) = sniff::route_uris(head, "Route") else {
                out.push(finding(Decision::Undecidable("a Route row no reader accepts")));
                continue;
            };
            let set = &req.dialog.route_set;
            let loose = set[0].loose;
            let request_uri = sniff::request_uri_facts(head)
                .map(|f| f.uri)
                .or_else(|| sniff::request_uri(head))
                .unwrap_or_default();
            let first_bad_hop = if loose {
                (!sent.is_empty() && sent.len() == set.len())
                    .then(|| {
                        sent.iter()
                            .zip(set.iter())
                            .position(|(a, e)| (&a.host, a.port) != (&e.host, e.port))
                    })
                    .flatten()
            } else {
                None
            };
            let reproduced = if loose {
                sent.len() == set.len() && first_bad_hop.is_none()
            } else {
                // §16.12: the first strict route rides the Request-URI, the
                // tail rides Route — with the real target legally appended.
                let tail: Vec<&String> = set[1..].iter().map(|r| &r.uri).collect();
                let carried: Vec<&String> = sent.iter().map(|r| &r.uri).collect();
                let tail_ok = carried == tail
                    || (carried.len() == tail.len() + 1 && carried[..tail.len()] == tail[..]);
                request_uri == set[0].uri && tail_ok
            };
            if reproduced {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::MidDialogRouteDiverged {
                route_msg: req.msg,
                route_hop: req.hop,
                route_ts_us: req.ts_us,
                method: req.method.to_string(),
                dialog_route_set: set.iter().map(|r| r.uri.clone()).collect(),
                sent_routes: sent.iter().map(|r| r.uri.clone()).collect(),
                loose_first_route: loose,
                request_uri,
                first_bad_hop,
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **§8.1.2 + RFC 3263 §4 — an in-dialog request's bytes go where its own
/// routing points.** With a non-empty loose route set §12.2.1.1 puts the next
/// hop as the topmost Route, and RFC 3263 §4 resolves that URI to a
/// `(host, port)`; behind a strict first route that route is the next hop
/// (§12.2.1.1 puts it in the Request-URI); with no Route rows the Request-URI
/// names it. This rule confirms the datagram left for that destination.
///
/// The occasion is ONE request the endpoint sent on a CONFIRMED dialog — before
/// the peer has a tag, §8.1.2 lets a UA send through a configured outbound
/// proxy, so there is nothing to charge. ACK and CANCEL follow the INVITE's
/// destination (§17 / §9.1), not the route set. Charges the sender.
///
/// The divergence this catches is a B2BUA pinned to an outbound proxy that
/// decouples the wire destination from the Route URI; a real UA's stack always
/// honours the derivation.
pub struct MidDialogWireDestination;

impl Obligation for MidDialogWireDestination {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for req in &seen.requests {
            if req.hop_by_hop() || req.initial_invite || !req.confirmed {
                continue;
            }
            let finding = |decision| req.finding(RuleId::MidDialogWireDestination, decision);
            let Some(head) = wire.msgs[req.msg].head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            // A Route line no reader accepts names no destination: judging the
            // wire against a URI nobody can resolve settles nothing.
            let Some(routes) = sniff::route_uris(head, "Route") else {
                out.push(finding(Decision::Undecidable("a Route row no reader accepts")));
                continue;
            };
            // Behind a strict first route the next hop is that route itself,
            // however the request was built: `MidDialogRoute` judges the build.
            let strict = req.dialog.route_set.first().filter(|r| !r.loose).cloned();
            let target = match strict.or_else(|| routes.into_iter().next()) {
                Some(first) => Some((first, true)),
                None => request_uri_target(head).map(|u| (u, false)),
            };
            let Some((target, from_route)) = target else {
                out.push(finding(Decision::Undecidable("no readable target at this vantage")));
                continue;
            };
            let Some(sent_to) = wire_addr(&wire.msgs[req.msg].dst) else {
                out.push(finding(Decision::Undecidable("no wire destination at this vantage")));
                continue;
            };
            if sent_to == (target.host.as_str().to_string(), target.port) {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::MidDialogWireTargetDiverged {
                wire_msg: req.msg,
                wire_hop: req.hop,
                wire_ts_us: req.ts_us,
                method: req.method.to_string(),
                sent_to: format!("{}:{}", sent_to.0, sent_to.1),
                target_uri: target.uri,
                target_host: target.host,
                target_port: target.port,
                from_route,
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The `(host, port)` an endpoint token names, or `None` where it is not an
/// address. A logical `#label` suffix is not part of the address.
pub(super) fn wire_addr(endpoint: &str) -> Option<(String, u16)> {
    let addr: std::net::SocketAddr = endpoint.split('#').next()?.parse().ok()?;
    Some((addr.ip().to_string(), addr.port()))
}

/// **§12.1.1 / §12.2.2 — Record-Route rides dialog-CREATING responses only.** A
/// 100 Trying creates no dialog, so a Record-Route on it is vestigial; and once
/// a dialog exists its route set is fixed, so a response to an IN-DIALOG request
/// cannot carry one either — there is no longer a set for it to join.
///
/// The occasion is ONE response the endpoint TOOK. Charges the endpoint that
/// sent it. A response whose request this vantage never carried settles nothing
/// (`Undecidable`): whether the transaction was in-dialog is exactly what
/// decides it.
///
/// **An ACK is never a correlation target.** It elicits no response, and a
/// non-2xx ACK reuses its INVITE's branch (§17.1.1.3) while carrying the final's
/// To tag — recording it would let a §17.2.1-retransmitted final that crosses
/// the ACK be misread as answering an "in-dialog ACK".
///
/// This is the teeth for a B2BUA or proxy that leaks Record-Route onto these
/// responses; the test UA ignores it, a strict UAC's route-set bookkeeping does
/// not.
pub struct RecordRoutePlacement;

impl Obligation for RecordRoutePlacement {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Requests each endpoint SENT, by transaction, with whether they were
        // in-dialog when they went out — built as the walk goes, so a response
        // is only ever measured against a request that preceded it.
        let mut sent: BTreeMap<(&str, &str, &str), (&str, bool)> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { method } => {
                    if !method.eq_ignore_ascii_case("ACK") {
                        if let Some(branch) = branch {
                            sent.insert(
                                (msg.src.as_str(), msg.call_id.as_str(), branch),
                                (
                                    method.as_str(),
                                    msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()),
                                ),
                            );
                        }
                    }
                }
                Kind::Response { status } => {
                    let finding = |decision| Finding {
                        rule: RuleId::RecordRoutePlacement,
                        emitter: msg.src.to_string(),
                        taker: msg.dst.to_string(),
                        cseq: msg.cseq,
                        relayed: false,
                        anchor: mi,
                        decision,
                    };
                    let Some(head) = msg.head.as_deref() else {
                        out.push(finding(Decision::Undecidable("no header block at this vantage")));
                        continue;
                    };
                    let rows = sniff::header_values(head, "Record-Route");
                    let Some(row) = rows.into_iter().next() else {
                        out.push(finding(Decision::Compliant));
                        continue;
                    };
                    let misplaced = |request_method: &str| {
                        Decision::Violated(Evidence::RecordRouteMisplaced {
                            record_route_msg: mi,
                            record_route_hop: msg.hop,
                            record_route_ts_us: msg.at_us,
                            status: *status,
                            record_route: row.clone(),
                            request_method: request_method.to_string(),
                        })
                    };
                    if *status == 100 {
                        out.push(finding(misplaced("")));
                        continue;
                    }
                    // The transaction this response answers is one the TAKER
                    // opened: it sent the request, and the response came back.
                    let opened =
                        branch.and_then(|b| sent.get(&(msg.dst.as_str(), msg.call_id.as_str(), b)));
                    match opened {
                        None => out.push(finding(Decision::Undecidable(
                            "no request on this transaction at this vantage",
                        ))),
                        Some((method, true)) => out.push(finding(misplaced(method))),
                        Some(_) => out.push(finding(Decision::Compliant)),
                    }
                }
            }
        }
        out
    }
}

/// How long an OPEN observation must keep running past an in-dialog request
/// before "the taker never answered it" reads as silence rather than
/// truncation. One second — the §17.2 reasoning of
/// [`super::capability::ANSWER_WINDOW_US`]; in a CLOSED observation it
/// collapses.
pub const UNKNOWN_DIALOG_WINDOW_US: u64 = 1_000_000;

/// **§12.2.2 — an in-dialog request for an unknown dialog is answered 481.** A
/// request carrying both tags claims to ride a dialog; a UAS that has no such
/// dialog cannot serve it — the state the request assumes does not exist — and
/// says so with 481 Call/Transaction Does Not Exist. Serving it instead (the
/// test UA answers whatever it is handed) hides a stale or mis-copied dialog id
/// that a real UAS would refuse.
///
/// The occasion is ONE in-dialog request the endpoint TOOK on a call where this
/// vantage carried at least one dialog-CREATING response for it — without that
/// there is nothing to say which dialogs the endpoint has, and the request is
/// not an occasion at all. Charges the taker; the 481 discharges it.
///
/// **A dialog is known by its PEER tag** — the dialog key the other four
/// rules use. It is the half the endpoint does not mint and the half that is
/// direction-independent, so one relay face that carried both directions of a
/// call knows both of its peers and is not charged for either. Naming the local
/// half instead would charge every such face.
///
/// **A non-2xx final to the establishing INVITE (sent without a To-tag) ends
/// the early dialog it rode (§12.3)**, so a request the UAS takes after sending
/// that final names no dialog. A dialog a 2xx accepted stays, and every other
/// tagged answer shows its dialog. A request is judged once, at its first
/// copy. A PRACK on the ended dialog is left to
/// [`super::prack::Prack2xxOr481`], which judges it by the `RSeq` it names.
///
/// **ACK and CANCEL are not occasions.** An ACK elicits no response at all
/// (§17.1.1.3), so it cannot be answered 481; a CANCEL is matched by
/// transaction, not by dialog (§9.1).
pub struct UnknownDialog481;

impl Obligation for UnknownDialog481 {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per endpoint: the peer tags whose dialog it has seen CONFIRMED on a
        // call, and whether that call ever carried a dialog-creating response
        // for it at all. Built as the walk goes, so a request is judged against
        // what the endpoint knew BEFORE it.
        let mut confirmed: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        let mut creating_seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        // The confirmed dialogs a 2xx to an INVITE accepted: no later non-2xx
        // final ends them.
        let mut accepted: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        // The early dialogs a non-2xx final ended.
        let mut ended: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        // `(Call-ID, branch)` of every INVITE sent without a To-tag: the
        // establishing transactions whose non-2xx final ends an early dialog.
        let mut establishing: BTreeSet<(&str, &str)> = BTreeSet::new();
        // Requests on an unknown dialog, awaiting the final the taker sent on
        // their transaction (0 = none yet).
        let mut unknown: Vec<usize> = Vec::new();
        let mut answers: BTreeMap<(&str, &str, &str), u16> = BTreeMap::new();
        let mut out: Vec<Finding> = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let call = msg.call_id.as_str();
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { method } => {
                    let hop_by_hop =
                        method.eq_ignore_ascii_case("ACK") || method.eq_ignore_ascii_case("CANCEL");
                    let from_tag = msg.from_tag.as_deref().unwrap_or_default();
                    let to_tag = msg.to_tag.as_deref().unwrap_or_default();
                    if method.eq_ignore_ascii_case("INVITE") && to_tag.is_empty() {
                        if let Some(branch) = branch {
                            establishing.insert((call, branch));
                        }
                    }
                    // A retransmission is the same request again, judged at
                    // its first copy.
                    if msg.repeat
                        || hop_by_hop
                        || from_tag.is_empty()
                        || to_tag.is_empty()
                        || call.is_empty()
                    {
                        continue;
                    }
                    let Some(branch) = branch else { continue };
                    let taker = msg.dst.as_str();
                    if !creating_seen.contains(&(taker, call)) {
                        continue; // nothing says which dialogs this endpoint has
                    }
                    if confirmed.contains(&(taker, call, from_tag)) {
                        out.push(finding_481(mi, msg, Decision::Compliant));
                        continue;
                    }
                    if method.eq_ignore_ascii_case("PRACK")
                        && ended.contains(&(taker, call, from_tag))
                    {
                        continue;
                    }
                    unknown.push(mi);
                    answers.entry((taker, call, branch)).or_insert(0);
                }
                Kind::Response { status } => {
                    // A tagged answer names both halves: the peer tag this
                    // endpoint learns, and the one it minted itself, and shows
                    // the dialog — except a non-2xx final on an establishing
                    // INVITE's branch, which ends the early dialog (§12.3)
                    // unless a 2xx accepted it.
                    let tagged = (200..300).contains(status)
                        || (*status > 100 && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()));
                    let both = msg.from_tag.as_deref().is_some_and(|t| !t.is_empty())
                        && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty());
                    let invite = msg.cseq_method.eq_ignore_ascii_case("INVITE");
                    let ends = *status >= 300
                        && invite
                        && branch.is_some_and(|b| establishing.contains(&(call, b)));
                    if tagged && both && !call.is_empty() {
                        for endpoint in [msg.src.as_str(), msg.dst.as_str()] {
                            let key =
                                (endpoint, call, remote_tag_of(msg, endpoint == msg.src.as_str()));
                            creating_seen.insert((endpoint, call));
                            if !ends {
                                confirmed.insert(key);
                                ended.remove(&key);
                                if invite && (200..300).contains(status) {
                                    accepted.insert(key);
                                }
                            } else if !accepted.contains(&key) {
                                confirmed.remove(&key);
                                ended.insert(key);
                            }
                        }
                    }
                    if *status >= 200 {
                        if let Some(branch) = branch {
                            if let Some(at) = answers.get_mut(&(msg.src.as_str(), call, branch)) {
                                if *at == 0 {
                                    *at = *status;
                                }
                            }
                        }
                    }
                }
            }
        }

        for mi in unknown {
            let msg = &wire.msgs[mi];
            let branch = msg.via_branch.as_deref().unwrap_or_default();
            let answered = answers
                .get(&(msg.dst.as_str(), msg.call_id.as_str(), branch))
                .copied()
                .unwrap_or(0);
            let decision = if answered == 481 {
                Decision::Compliant
            } else if answered == 0
                && !wire.obs.absence_decidable(msg.at_us, UNKNOWN_DIALOG_WINDOW_US)
            {
                Decision::Undecidable(
                    "the observation stopped inside the window — truncation, not silence",
                )
            } else {
                let method = match &msg.kind {
                    Kind::Request { method } => method.as_str(),
                    Kind::Response { .. } => "",
                };
                Decision::Violated(Evidence::UnknownDialogRequest {
                    unknown_dialog_msg: mi,
                    unknown_dialog_hop: msg.hop,
                    unknown_dialog_ts_us: msg.at_us,
                    method: method.to_string(),
                    from_tag: msg.from_tag.clone().unwrap_or_default(),
                    to_tag: msg.to_tag.clone().unwrap_or_default(),
                    answered_status: answered,
                })
            };
            out.push(finding_481(mi, msg, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// How long after a non-2xx final this vantage saw the caller take her BYE
/// may still have crossed it in flight; one second, as
/// [`UNKNOWN_DIALOG_WINDOW_US`].
pub const BYE_CROSSING_WINDOW_US: u64 = 1_000_000;

/// **§15 — a BYE names a dialog, and never an early one the sender is callee
/// of.** BYE ends a session; a request with no dialog to end is answered 481 by
/// a real peer, and the callee of a dialog it has not yet accepted has other
/// words for "no" — a 4xx/5xx/6xx to the pending INVITE, or (as UAC) a CANCEL.
/// BYE-ing an early dialog leaves the caller's INVITE transaction with no final
/// at all. The test UA answers a BYE whatever state it is in, so nothing else
/// catches either shape.
///
/// The occasion is ONE BYE an endpoint SENT, fresh (a retransmission is the
/// same act again). Charges that endpoint.
///
/// **Three ways to fail it, one obligation.** A BYE carrying no peer tag names
/// no dialog at all; a BYE whose sender ANSWERED the dialog-establishing INVITE
/// but never accepted it ends a dialog that is still early; a BYE the caller
/// sends more than [`BYE_CROSSING_WINDOW_US`] after this vantage saw her take
/// a non-2xx final to that INVITE, and no 2xx before it, names the early
/// dialog the final ended
/// (§12.3) — inside the window it is undecidable. Everything else —
/// including a BYE the caller sends, whose own INVITE the dialog rests on — is
/// compliant.
///
/// **The dialog is known by its PEER tag**, so a callee-initiated BYE, whose
/// From/To are reversed relative to the establishing INVITE, still rides the
/// dialog it belongs to. Being that dialog's UAS is read off the establishing
/// INVITE's own branch: a UAC answering its peer's later re-INVITE has answered
/// an INVITE, but not THE INVITE, and is not the callee of anything.
pub struct NoByeOutsideOrEarlyDialog;

impl Obligation for NoByeOutsideOrEarlyDialog {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for bye in &seen.requests {
            if !bye.method.eq_ignore_ascii_case("BYE") || wire.msgs[bye.msg].repeat {
                continue;
            }
            let msg = &wire.msgs[bye.msg];
            let evidence = |early_dialog| {
                Decision::Violated(Evidence::ByeOffDialog {
                    bye_msg: bye.msg,
                    bye_hop: bye.hop,
                    bye_ts_us: bye.ts_us,
                    from_tag: msg.from_tag.clone().unwrap_or_default(),
                    to_tag: msg.to_tag.clone().unwrap_or_default(),
                    early_dialog,
                })
            };
            // No peer tag: the BYE names no dialog this endpoint is in.
            if !bye.confirmed {
                out.push(bye.finding(RuleId::NoByeOutsideOrEarlyDialog, evidence(false)));
                continue;
            }
            // The callee of a dialog it never accepted owes a rejection or a
            // CANCEL, not a BYE.
            let early =
                bye.dialog.answered_establishing_invite && !bye.dialog.accepted_establishing_invite;
            // The caller whose INVITE took a non-2xx final has no early dialog
            // left (§12.3); a BYE inside the window may have crossed it.
            let decision = match bye.dialog.rejected_at_us {
                _ if early => evidence(true),
                Some(at) if bye.ts_us > at.saturating_add(BYE_CROSSING_WINDOW_US) => {
                    evidence(false)
                }
                Some(at) if bye.ts_us >= at => Decision::Undecidable(
                    "the BYE may have crossed the final that ended its early dialog",
                ),
                _ => Decision::Compliant,
            };
            out.push(bye.finding(RuleId::NoByeOutsideOrEarlyDialog, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One §12.2.2 occasion, charged to the endpoint that TOOK the request — the
/// UAS that owed the 481 — with its sender as the taker of that answer.
fn finding_481(mi: usize, msg: &Msg, decision: Decision) -> Finding {
    Finding {
        rule: RuleId::UnknownDialog481,
        emitter: msg.dst.to_string(),
        taker: msg.src.to_string(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}

// ---------------------------------------------------------------------------
// The dialog walk the three §12.2.1.1 rules share
// ---------------------------------------------------------------------------

/// One endpoint's view of ONE dialog. Spelled out as a struct so the three
/// parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DialogKey<'a> {
    /// The endpoint whose dialog this is.
    endpoint: &'a str,
    call_id: &'a str,
    /// The PEER's dialog tag, `""` before the peer has minted one.
    remote_tag: &'a str,
}

/// What the establishing exchange fixed for one dialog, as one endpoint saw it.
#[derive(Debug, Default, Clone)]
struct Dialog {
    /// The URI this endpoint answers on, and the URI its peer does — §12.2.1.1
    /// has every in-dialog request repeat the pair.
    local_uri: String,
    remote_uri: String,
    /// The route set in the order in-dialog requests must reproduce it.
    route_set: Vec<UriFacts>,
    /// The endpoint sent the establishing INVITE, so its route set is the
    /// dialog-creating RESPONSE's Record-Route stack reversed (§12.1.2).
    is_uac: bool,
    /// Top-Via branches of the establishing INVITE, in each direction. An
    /// INVITE on the sent branch is that INVITE, not a re-INVITE.
    initial_invite_sent_branch: String,
    initial_invite_received_branch: String,
    /// The endpoint ANSWERED the establishing INVITE with a tagged response, so
    /// it is this dialog's UAS — the party §15 forbids a BYE to before it has
    /// accepted.
    answered_establishing_invite: bool,
    /// One of those answers was a 2xx: the dialog it is the UAS of is
    /// confirmed, not early.
    accepted_establishing_invite: bool,
    /// The endpoint, as UAC, took a 2xx on the establishing INVITE's branch:
    /// the dialog is confirmed, and a non-2xx after it on that branch is a
    /// stray that ends nothing (§13.2.2.4, §17.1.1.2).
    took_establishing_2xx: bool,
    /// When the endpoint, as UAC, took a non-2xx final on the establishing
    /// INVITE's branch before any 2xx on it (§12.3 ends the early dialog);
    /// cleared by any INVITE 2xx it takes and by a provisional on a later
    /// INVITE (a challenge retried).
    rejected_at_us: Option<u64>,
}

/// One request an endpoint SENT, with the dialog state that held when it went.
#[derive(Debug)]
struct Sent<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    /// The request-line method, as the wire spelled it.
    method: &'a str,
    emitter: &'a str,
    taker: &'a str,
    dialog: Dialog,
    /// This IS the dialog-creating INVITE: the first establishing INVITE this
    /// endpoint's view of the dialog carried, in either direction.
    initial_invite: bool,
    /// §12.2.1.1's in-dialog test: both tags present, and — for an INVITE — not
    /// the establishing INVITE by branch.
    in_dialog: bool,
    /// The peer's tag is known, so the dialog is confirmed at this endpoint.
    confirmed: bool,
}

impl<'a> Sent<'a> {
    fn finding(&self, rule: RuleId, decision: Decision) -> Finding {
        Finding {
            rule,
            emitter: self.emitter.to_string(),
            taker: self.taker.to_string(),
            cseq: self.cseq,
            relayed: false,
            anchor: self.msg,
            decision,
        }
    }

    /// ACK and CANCEL take the INVITE's own path (§17.1.1.3 / §9.1), not the
    /// dialog's — the route rules do not judge them.
    fn hop_by_hop(&self) -> bool {
        self.method.eq_ignore_ascii_case("ACK") || self.method.eq_ignore_ascii_case("CANCEL")
    }
}

/// What one view's messages say about the dialogs on it: every request each
/// endpoint sent, paired with the dialog state that held when it did.
#[derive(Debug, Default)]
struct Reading<'a> {
    requests: Vec<Sent<'a>>,
    /// Per dialog, keyed as §12 names it.
    dialogs: BTreeMap<DialogKey<'a>, Dialog>,
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            // A message with no call or no From tag names no dialog: it can key
            // nothing, in either endpoint's view.
            if msg.call_id.is_empty() || msg.from_tag.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            for endpoint in [msg.src.as_str(), msg.dst.as_str()] {
                seen.absorb(mi, msg, endpoint, endpoint == msg.src.as_str());
                if msg.src == msg.dst {
                    break; // one endpoint talking to itself is one view
                }
            }
        }
        seen
    }

    /// Absorb one message into `endpoint`'s view of the dialog it rides:
    /// record the occasion it opens (a request the endpoint sent), then let it
    /// advance the dialog state — the order the check demands, since a request
    /// is judged against what the dialog was BEFORE it.
    fn absorb(&mut self, mi: usize, msg: &'a Msg, endpoint: &'a str, sent: bool) {
        let remote_tag = remote_tag_of(msg, sent);
        let key = DialogKey { endpoint, call_id: msg.call_id.as_str(), remote_tag };
        // A tag first seen now inherits what the pending state fixed: the
        // establishing INVITE the UAC sent before its peer had a tag.
        if !remote_tag.is_empty() && !self.dialogs.contains_key(&key) {
            let pending =
                self.dialogs.get(&DialogKey { remote_tag: "", ..key }).cloned().unwrap_or_default();
            self.dialogs.insert(key, pending);
        }
        let dialog = self.dialogs.entry(key).or_default();

        if let Kind::Request { method } = &msg.kind {
            let branch = msg.via_branch.as_deref().unwrap_or_default();
            let establishing = method.eq_ignore_ascii_case("INVITE")
                && msg.to_tag.as_deref().is_none_or(str::is_empty);
            if sent {
                let initial_invite = method.eq_ignore_ascii_case("INVITE")
                    && dialog.initial_invite_sent_branch.is_empty()
                    && dialog.initial_invite_received_branch.is_empty();
                let both_tags = !remote_tag.is_empty()
                    && msg.from_tag.as_deref().is_some_and(|t| !t.is_empty())
                    && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty());
                let in_dialog = both_tags
                    && !(method.eq_ignore_ascii_case("INVITE")
                        && !branch.is_empty()
                        && branch == dialog.initial_invite_sent_branch);
                self.requests.push(Sent {
                    msg: mi,
                    hop: msg.hop,
                    ts_us: msg.at_us,
                    cseq: msg.cseq,
                    method: method.as_str(),
                    emitter: msg.src.as_str(),
                    taker: msg.dst.as_str(),
                    dialog: dialog.clone(),
                    initial_invite,
                    in_dialog,
                    confirmed: !remote_tag.is_empty(),
                });
            }
            // Only the establishing INVITE fixes role, URIs and route set.
            if !establishing {
                return;
            }
            if sent {
                if dialog.initial_invite_sent_branch.is_empty() {
                    dialog.is_uac = true;
                    dialog.initial_invite_sent_branch = branch.to_string();
                    set_if_empty(&mut dialog.local_uri, msg, "From");
                    set_if_empty(&mut dialog.remote_uri, msg, "To");
                }
            } else if dialog.initial_invite_received_branch.is_empty() {
                dialog.initial_invite_received_branch = branch.to_string();
                set_if_empty(&mut dialog.local_uri, msg, "To");
                set_if_empty(&mut dialog.remote_uri, msg, "From");
                if dialog.route_set.is_empty() {
                    // §12.1.1: the UAS's route set is the recorded stack in
                    // wire order.
                    dialog.route_set = record_route_set(msg);
                }
            }
            return;
        }

        let Kind::Response { status } = &msg.kind else { return };

        // The UAC's own reading of its establishing INVITE's outcome.
        if !sent && msg.cseq_method.eq_ignore_ascii_case("INVITE") {
            let on_initial = !dialog.initial_invite_sent_branch.is_empty()
                && msg.via_branch.as_deref() == Some(dialog.initial_invite_sent_branch.as_str());
            dialog.took_establishing_2xx |= on_initial && (200..300).contains(status);
            if (200..300).contains(status) || (*status > 100 && *status < 200 && !on_initial) {
                dialog.rejected_at_us = None;
            } else if *status >= 300 && on_initial && !dialog.took_establishing_2xx {
                dialog.rejected_at_us.get_or_insert(msg.at_us);
            }
        }

        // A tagged answer the endpoint SENT on the establishing INVITE's own
        // transaction makes it this dialog's UAS (§15's early-BYE test): the
        // branch is what says the answer is to THAT INVITE and not a later
        // re-INVITE the endpoint happens to be answering.
        if sent
            && msg.cseq_method.eq_ignore_ascii_case("INVITE")
            && !dialog.initial_invite_received_branch.is_empty()
            && msg.via_branch.as_deref() == Some(dialog.initial_invite_received_branch.as_str())
            && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty())
        {
            dialog.answered_establishing_invite = true;
            dialog.accepted_establishing_invite |= (200..300).contains(status);
        }

        // Only the UAC's route set is still open, and only a dialog-CREATING
        // answer to the INVITE closes it (§12.1.2).
        if sent || !dialog.is_uac || !dialog.route_set.is_empty() {
            return;
        }
        let creating = (200..300).contains(status)
            || (*status > 100
                && *status < 200
                && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()));
        if creating && msg.cseq_method.eq_ignore_ascii_case("INVITE") {
            let mut set = record_route_set(msg);
            set.reverse();
            dialog.route_set = set;
        }
    }
}

#[cfg(test)]
mod tests;
