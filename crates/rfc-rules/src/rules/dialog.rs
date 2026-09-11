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
    fn id(&self) -> RuleId {
        RuleId::MidDialogUri
    }

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
    fn id(&self) -> RuleId {
        RuleId::MidDialogRoute
    }

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
/// `(host, port)`; with no Route rows the Request-URI names it. This rule
/// confirms the datagram left for that destination.
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
    fn id(&self) -> RuleId {
        RuleId::MidDialogWireDestination
    }

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
            let target = match routes.into_iter().next() {
                Some(first) => Some((first, true)),
                None => sniff::request_uri_facts(head).map(|u| (u, false)),
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
fn wire_addr(endpoint: &str) -> Option<(String, u16)> {
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
    fn id(&self) -> RuleId {
        RuleId::RecordRoutePlacement
    }

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
/// **ACK and CANCEL are not occasions.** An ACK elicits no response at all
/// (§17.1.1.3), so it cannot be answered 481; a CANCEL is matched by
/// transaction, not by dialog (§9.1).
pub struct UnknownDialog481;

impl Obligation for UnknownDialog481 {
    fn id(&self) -> RuleId {
        RuleId::UnknownDialog481
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per endpoint: the peer tags whose dialog it has seen CONFIRMED on a
        // call, and whether that call ever carried a dialog-creating response
        // for it at all. Built as the walk goes, so a request is judged against
        // what the endpoint knew BEFORE it.
        let mut confirmed: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        let mut creating_seen: BTreeSet<(&str, &str)> = BTreeSet::new();
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
                    if hop_by_hop || from_tag.is_empty() || to_tag.is_empty() || call.is_empty() {
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
                    unknown.push(mi);
                    answers.entry((taker, call, branch)).or_insert(0);
                }
                Kind::Response { status } => {
                    // A dialog-creating answer names both halves: the peer tag
                    // this endpoint learns, and the one it minted itself.
                    let creating = (200..300).contains(status)
                        || (*status > 100 && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()));
                    let both = msg.from_tag.as_deref().is_some_and(|t| !t.is_empty())
                        && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty());
                    if creating && both && !call.is_empty() {
                        for endpoint in [msg.src.as_str(), msg.dst.as_str()] {
                            creating_seen.insert((endpoint, call));
                            confirmed.insert((
                                endpoint,
                                call,
                                remote_tag_of(msg, endpoint == msg.src.as_str()),
                            ));
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
/// **Two ways to fail it, one obligation.** A BYE carrying no peer tag names no
/// dialog at all; a BYE whose sender ANSWERED the dialog-establishing INVITE
/// but never accepted it ends a dialog that is still early. Everything else —
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
    fn id(&self) -> RuleId {
        RuleId::NoByeOutsideOrEarlyDialog
    }

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
            out.push(bye.finding(
                RuleId::NoByeOutsideOrEarlyDialog,
                if early { evidence(true) } else { Decision::Compliant },
            ));
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

/// The PEER's dialog tag on `msg` as `endpoint` sees it. A request names its
/// peer in the header the OTHER side owns: To when the endpoint sent it, From
/// when the endpoint took it; a response is the mirror. `""` where the peer has
/// not minted one yet.
fn remote_tag_of(msg: &Msg, sent: bool) -> &str {
    let peer_is_to = match &msg.kind {
        Kind::Request { .. } => sent,
        Kind::Response { .. } => !sent,
    };
    let tag = if peer_is_to { msg.to_tag.as_deref() } else { msg.from_tag.as_deref() };
    tag.unwrap_or_default()
}

/// Fill `slot` from `msg`'s address header when it is still empty and the
/// header reads — the first establishing INVITE fixes the pair.
fn set_if_empty(slot: &mut String, msg: &Msg, header: &str) {
    if !slot.is_empty() {
        return;
    }
    if let Some(uri) = msg.head.as_deref().and_then(|h| sniff::name_addr_uri(h, header)) {
        *slot = uri;
    }
}

/// The Record-Route stack `msg` carries, in wire order. A row no reader accepts
/// contributes nothing: an unreadable header is the grammar rules' finding.
fn record_route_set(msg: &Msg) -> Vec<UriFacts> {
    msg.head.as_deref().and_then(|h| sniff::route_uris(h, "Record-Route")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! The rules' OWN semantics under a CLOSED observation: which messages fix
    //! a dialog, what an in-dialog request then owes it, and where the wire
    //! settles nothing.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        MidDialogRoute, MidDialogUri, MidDialogWireDestination, NoByeOutsideOrEarlyDialog,
        RecordRoutePlacement, UnknownDialog481,
    };

    const ALICE: &str = "127.0.0.1:5060";
    const BOB: &str = "127.0.0.1:5070";
    const A_URI: &str = "sip:alice@127.0.0.1";
    const B_URI: &str = "sip:bob@127.0.0.1";

    /// A request on the wire between two endpoints, with a caller-controlled
    /// header block — the §12 rules read From/To/Route off the bytes.
    #[allow(clippy::too_many_arguments)]
    fn req(
        at_us: u64,
        src: &str,
        dst: &str,
        method: &str,
        r_uri: &str,
        branch: &str,
        cseq: u32,
        from: &str,
        to: &str,
        to_tag: Option<&str>,
        extra: &str,
    ) -> Msg {
        let to_hdr = match to_tag {
            Some(t) => format!("<{to}>;tag={t}"),
            None => format!("<{to}>"),
        };
        let head = format!(
            "{method} {r_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{from}>;tag=at\r\n\
             To: {to_hdr}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        );
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: method.to_string(),
            from_tag: Some("at".to_string()),
            to_tag: to_tag.map(str::to_string),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// Alice's dialog-creating INVITE toward bob.
    fn invite(at_us: u64, branch: &str, extra: &str) -> Msg {
        req(at_us, ALICE, BOB, "INVITE", B_URI, branch, 1, A_URI, B_URI, None, extra)
    }

    /// A response bob sent alice.
    fn rsp(at_us: u64, status: u16, cseq: u32, method: &str, branch: &str, extra: &str) -> Msg {
        rsp_tagged(at_us, status, cseq, method, branch, Some("bt"), extra)
    }

    fn rsp_tagged(
        at_us: u64,
        status: u16,
        cseq: u32,
        method: &str,
        branch: &str,
        to_tag: Option<&str>,
        extra: &str,
    ) -> Msg {
        let to_hdr = match to_tag {
            Some(t) => format!("<{B_URI}>;tag={t}"),
            None => format!("<{B_URI}>"),
        };
        let head = format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A_URI}>;tag=at\r\n\
             To: {to_hdr}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        );
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
            from_tag: Some("at".to_string()),
            to_tag: to_tag.map(str::to_string),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
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

    fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        eval(rule, msgs).into_iter().filter(Finding::violated).collect()
    }

    // ── MidDialogUri (RFC 3261 §12.2.1.1) ───────────────────────────────────

    /// A BYE that keeps the dialog's URIs is the obligation met, and it is one
    /// occasion — the rule charges the endpoint that sent it.
    #[test]
    fn an_in_dialog_request_repeating_the_dialog_uris_is_clean() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
        ];
        let all = eval(&MidDialogUri, &msgs);
        assert_eq!(all.len(), 1, "one occasion, the BYE: {all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
        assert_eq!(all[0].emitter, ALICE, "the endpoint that sent it is charged");
        assert_eq!(all[0].taker, BOB);
    }

    /// A BYE that rewrites the From URI mid-dialog: ONE finding naming both
    /// URIs, since rewriting the dialog's identity is one act.
    #[test]
    fn a_rewritten_from_uri_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(
                3_000,
                ALICE,
                BOB,
                "BYE",
                B_URI,
                "z9hG4bK-b",
                2,
                "sip:eve@127.0.0.1",
                B_URI,
                Some("bt"),
                "",
            ),
        ];
        let f = hits(&MidDialogUri, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::MidDialogUriChanged {
            method,
            sent_from_uri,
            dialog_local_uri,
            sent_to_uri,
            dialog_remote_uri,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(method.as_str(), "BYE");
        assert_eq!(sent_from_uri.as_str(), "sip:eve@127.0.0.1");
        assert_eq!(dialog_local_uri.as_str(), A_URI);
        assert_eq!(sent_to_uri, dialog_remote_uri, "the To URI held");
    }

    /// Both URIs rewritten is still ONE occasion — the collapse the report then
    /// spells out.
    #[test]
    fn both_uris_rewritten_are_one_finding() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(
                3_000,
                ALICE,
                BOB,
                "BYE",
                B_URI,
                "z9hG4bK-b",
                2,
                "sip:eve@127.0.0.1",
                "sip:mallory@127.0.0.1",
                Some("bt"),
                "",
            ),
        ];
        let f = hits(&MidDialogUri, &msgs);
        assert_eq!(f.len(), 1, "one occasion, both URIs on it: {f:?}");
        let Decision::Violated(Evidence::MidDialogUriChanged {
            sent_from_uri,
            dialog_local_uri,
            sent_to_uri,
            dialog_remote_uri,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_ne!(sent_from_uri, dialog_local_uri);
        assert_ne!(sent_to_uri, dialog_remote_uri);
    }

    /// A dialog whose creation this vantage never carried owes URIs nobody can
    /// name — undecidable, never a guess.
    #[test]
    fn an_in_dialog_request_on_an_unseen_dialog_is_undecidable() {
        let msgs =
            [req(1_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), "")];
        let f = eval(&MidDialogUri, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(
                f[0].decision,
                Decision::Undecidable("no dialog-creating INVITE at this vantage")
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// The dialog-creating INVITE and a CANCEL are not in-dialog requests: the
    /// first precedes the dialog, the second is hop-by-hop (§9.1).
    #[test]
    fn the_establishing_invite_and_a_cancel_open_no_occasion() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            req(2_000, ALICE, BOB, "CANCEL", B_URI, "z9hG4bK-i", 1, A_URI, B_URI, Some("bt"), ""),
        ];
        assert!(eval(&MidDialogUri, &msgs).is_empty(), "{:?}", eval(&MidDialogUri, &msgs));
    }

    /// Forking (§12.1.2): one INVITE, two callee tags. The establishing INVITE
    /// is replicated into BOTH dialogs, so a request on either fork is judged
    /// against the URIs that one INVITE fixed.
    #[test]
    fn a_forked_dialog_inherits_the_establishing_invites_uris() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp_tagged(2_000, 180, 1, "INVITE", "z9hG4bK-i", Some("b1"), ""),
            rsp_tagged(3_000, 200, 1, "INVITE", "z9hG4bK-i", Some("b2"), ""),
            req(
                4_000,
                ALICE,
                BOB,
                "BYE",
                B_URI,
                "z9hG4bK-b2",
                2,
                "sip:eve@127.0.0.1",
                B_URI,
                Some("b2"),
                "",
            ),
        ];
        let f = hits(&MidDialogUri, &msgs);
        assert_eq!(f.len(), 1, "the second fork knows the dialog's URIs too: {f:?}");
    }

    // ── MidDialogRoute (RFC 3261 §12.2.1.1 / §16.12) ────────────────────────

    /// The UAC's route set is the 200's Record-Route stack REVERSED: two rows
    /// `p1, p2` owe Route rows `p2, p1`.
    #[test]
    fn a_replayed_loose_route_set_is_clean() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(
                2_000,
                200,
                1,
                "INVITE",
                "z9hG4bK-i",
                "Record-Route: <sip:p1@127.0.0.1;lr>\r\nRecord-Route: <sip:p2@127.0.0.1;lr>\r\n",
            ),
            req(
                3_000,
                ALICE,
                BOB,
                "BYE",
                B_URI,
                "z9hG4bK-b",
                2,
                A_URI,
                B_URI,
                Some("bt"),
                "Route: <sip:p2@127.0.0.1;lr>\r\nRoute: <sip:p1@127.0.0.1;lr>\r\n",
            ),
        ];
        let all = eval(&MidDialogRoute, &msgs);
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    }

    /// Dropping the Route rows on a dialog with a non-empty set is the defect.
    #[test]
    fn an_omitted_route_set_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
            req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
        ];
        let f = hits(&MidDialogRoute, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::MidDialogRouteDiverged {
            dialog_route_set,
            sent_routes,
            loose_first_route,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert!(loose_first_route);
        assert_eq!(dialog_route_set.len(), 1);
        assert!(sent_routes.is_empty());
    }

    /// A proxy legitimately rewrites its Record-Route URI PARAMETERS per
    /// direction; only the routing-significant `host:port` is compared, so a
    /// re-parameterised hop is clean and a re-AIMED one is not.
    #[test]
    fn a_reparameterised_hop_is_clean_and_a_reaimed_one_is_not() {
        let dialog = |route: &str| {
            [
                invite(1_000, "z9hG4bK-i", ""),
                rsp(
                    2_000,
                    200,
                    1,
                    "INVITE",
                    "z9hG4bK-i",
                    "Record-Route: <sip:p1@127.0.0.1;lr;e=abc>\r\n",
                ),
                req(
                    3_000,
                    ALICE,
                    BOB,
                    "BYE",
                    B_URI,
                    "z9hG4bK-b",
                    2,
                    A_URI,
                    B_URI,
                    Some("bt"),
                    route,
                ),
            ]
        };
        assert!(
            hits(&MidDialogRoute, &dialog("Route: <sip:p1@127.0.0.1;lr;outbound>\r\n")).is_empty(),
            "per-direction parameters are not a route change",
        );
        let f = hits(&MidDialogRoute, &dialog("Route: <sip:p9@127.0.0.2;lr;e=abc>\r\n"));
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::MidDialogRouteDiverged { first_bad_hop, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*first_bad_hop, Some(0));
    }

    /// A STRICT first route (§16.12) rides the Request-URI, and the real target
    /// is legally appended to the Route tail.
    #[test]
    fn a_strict_route_set_rides_the_request_uri() {
        let dialog = |r_uri: &str, route: &str| {
            [
                invite(1_000, "z9hG4bK-i", ""),
                rsp(
                    2_000,
                    200,
                    1,
                    "INVITE",
                    "z9hG4bK-i",
                    "Record-Route: <sip:p2@127.0.0.1>\r\nRecord-Route: <sip:p1@127.0.0.1>\r\n",
                ),
                req(
                    3_000,
                    ALICE,
                    BOB,
                    "BYE",
                    r_uri,
                    "z9hG4bK-b",
                    2,
                    A_URI,
                    B_URI,
                    Some("bt"),
                    route,
                ),
            ]
        };
        // Reversed set = [p1, p2]: p1 becomes the Request-URI, p2 + the target
        // ride Route.
        assert!(
            hits(
                &MidDialogRoute,
                &dialog(
                    "sip:p1@127.0.0.1",
                    "Route: <sip:p2@127.0.0.1>\r\nRoute: <sip:bob@127.0.0.1>\r\n"
                )
            )
            .is_empty(),
            "the target appended to the tail is §16.12's own form",
        );
        let f = hits(
            &MidDialogRoute,
            &dialog(B_URI, "Route: <sip:p2@127.0.0.1>\r\nRoute: <sip:bob@127.0.0.1>\r\n"),
        );
        assert_eq!(f.len(), 1, "a Request-URI that is not the first strict route: {f:?}");
        let Decision::Violated(Evidence::MidDialogRouteDiverged {
            loose_first_route,
            request_uri,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert!(!loose_first_route);
        assert_eq!(request_uri.as_str(), B_URI);
    }

    /// A Route row no reader accepts names no path: the routing rule states
    /// nothing, and the grammar rules own the row.
    #[test]
    fn an_unreadable_route_row_is_undecidable() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
            req(
                3_000,
                ALICE,
                BOB,
                "BYE",
                B_URI,
                "z9hG4bK-b",
                2,
                A_URI,
                B_URI,
                Some("bt"),
                "Route: <sip:p1@127.0.0.1;lr\r\n",
            ),
        ];
        let f = eval(&MidDialogRoute, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(f[0].decision, Decision::Undecidable("a Route row no reader accepts")),
            "{:?}",
            f[0].decision
        );
    }

    /// An empty route set asks for nothing, and ACK/CANCEL take the INVITE's
    /// own path — no occasion in either case.
    #[test]
    fn an_empty_route_set_and_the_hop_by_hop_methods_open_no_occasion() {
        let no_set = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
        ];
        assert!(eval(&MidDialogRoute, &no_set).is_empty());

        let acked = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
            req(3_000, ALICE, BOB, "ACK", B_URI, "z9hG4bK-a", 1, A_URI, B_URI, Some("bt"), ""),
        ];
        assert!(eval(&MidDialogRoute, &acked).is_empty());
    }

    // ── MidDialogWireDestination (RFC 3261 §8.1.2 + RFC 3263 §4) ────────────

    /// With no Route rows the Request-URI names the destination; the bytes went
    /// there.
    #[test]
    fn bytes_sent_to_the_request_uri_are_clean() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(
                3_000,
                ALICE,
                BOB,
                "BYE",
                "sip:bob@127.0.0.1:5070",
                "z9hG4bK-b",
                2,
                A_URI,
                B_URI,
                Some("bt"),
                "",
            ),
        ];
        let all = eval(&MidDialogWireDestination, &msgs);
        assert_eq!(all.len(), 1, "{all:?}");
        assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
    }

    /// The bytes left for a destination the request's own routing never named.
    #[test]
    fn bytes_sent_past_the_derived_destination_are_violated() {
        let mut bye = req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            "sip:bob@127.0.0.1:5070",
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "",
        );
        bye.dst = "127.0.0.1:9999".to_string();
        let msgs =
            [invite(1_000, "z9hG4bK-i", ""), rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""), bye];
        let f = hits(&MidDialogWireDestination, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::MidDialogWireTargetDiverged {
            sent_to,
            target_host,
            target_port,
            from_route,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(sent_to.as_str(), "127.0.0.1:9999");
        assert_eq!((target_host.as_str(), *target_port), ("127.0.0.1", 5070));
        assert!(!from_route, "with no Route rows the Request-URI named it");
    }

    /// A non-empty Route set moves the derivation to the TOPMOST Route.
    #[test]
    fn the_topmost_route_names_the_destination() {
        let mut bye = req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            "sip:bob@127.0.0.1:5070",
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "Route: <sip:p1@127.0.0.1:5090;lr>\r\n",
        );
        bye.dst = "127.0.0.1:5090".to_string();
        let msgs =
            [invite(1_000, "z9hG4bK-i", ""), rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""), bye];
        assert!(hits(&MidDialogWireDestination, &msgs).is_empty(), "the Route hop is the target");
    }

    /// An unconfirmed dialog may still be going through a configured outbound
    /// proxy (§8.1.2): no occasion before the peer has a tag.
    #[test]
    fn an_unconfirmed_dialog_opens_no_occasion() {
        let msgs = [invite(1_000, "z9hG4bK-i", "")];
        assert!(eval(&MidDialogWireDestination, &msgs).is_empty());
    }

    // ── RecordRoutePlacement (RFC 3261 §12.1.1 / §12.2.2) ───────────────────

    /// Record-Route on the dialog-CREATING response is where it belongs.
    #[test]
    fn record_route_on_the_dialog_creating_response_is_clean() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
        ];
        assert!(hits(&RecordRoutePlacement, &msgs).is_empty());
    }

    /// A 100 creates no dialog, so a Record-Route on it is vestigial — and the
    /// finding charges the endpoint that SENT the 100, reported at its taker.
    #[test]
    fn record_route_on_a_100_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp_tagged(
                2_000,
                100,
                1,
                "INVITE",
                "z9hG4bK-i",
                None,
                "Record-Route: <sip:p1@127.0.0.1;lr>\r\n",
            ),
        ];
        let f = hits(&RecordRoutePlacement, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, BOB, "the endpoint that sent the 100 is charged");
        assert_eq!(f[0].taker, ALICE, "read at the endpoint that took it");
        let Decision::Violated(Evidence::RecordRouteMisplaced {
            status,
            record_route,
            request_method,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(*status, 100);
        assert_eq!(record_route.as_str(), "<sip:p1@127.0.0.1;lr>");
        assert!(request_method.is_empty(), "a 100 is vestigial whatever it answers");
    }

    /// The route set is fixed at establishment: a 200 to an in-dialog re-INVITE
    /// cannot carry Record-Route.
    #[test]
    fn record_route_on_a_response_to_an_in_dialog_request_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            req(3_000, ALICE, BOB, "INVITE", B_URI, "z9hG4bK-r", 2, A_URI, B_URI, Some("bt"), ""),
            rsp(4_000, 200, 2, "INVITE", "z9hG4bK-r", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
        ];
        let f = hits(&RecordRoutePlacement, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::RecordRouteMisplaced { request_method, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(request_method.as_str(), "INVITE");
    }

    /// §17.1.1.3: the ACK for a non-2xx final reuses the INVITE's branch and
    /// carries the final's To tag. A §17.2.1 retransmit landing after it must
    /// still read as answering the INVITE — an ACK elicits no response, so it is
    /// never a correlation target.
    #[test]
    fn a_retransmitted_final_crossing_its_ack_still_answers_the_invite() {
        let final_480 = |at_us| {
            rsp(at_us, 480, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n")
        };
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            final_480(2_000),
            req(3_000, ALICE, BOB, "ACK", B_URI, "z9hG4bK-i", 1, A_URI, B_URI, Some("bt"), ""),
            final_480(4_000),
        ];
        assert!(
            hits(&RecordRoutePlacement, &msgs).is_empty(),
            "{:?}",
            hits(&RecordRoutePlacement, &msgs)
        );
    }

    /// A response on a transaction this vantage never carried the request for
    /// settles nothing: whether it was in-dialog is exactly the open question.
    #[test]
    fn a_record_route_on_an_unplaceable_response_is_undecidable() {
        let msgs = [rsp(1_000, 200, 9, "OPTIONS", "z9hG4bK-x", "Record-Route: <sip:p1@h;lr>\r\n")];
        let f = eval(&RecordRoutePlacement, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            matches!(
                f[0].decision,
                Decision::Undecidable("no request on this transaction at this vantage")
            ),
            "{:?}",
            f[0].decision
        );
    }

    // ── UnknownDialog481 (RFC 3261 §12.2.2) ─────────────────────────────────

    /// An in-dialog request alice sent bob, with a caller-chosen tag pair.
    fn in_dialog(at_us: u64, method: &str, branch: &str, cseq: u32, ft: &str, tt: &str) -> Msg {
        let head = format!(
            "{method} {B_URI} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A_URI}>;tag={ft}\r\n\
             To: <{B_URI}>;tag={tt}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        );
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
            from_tag: Some(ft.to_string()),
            to_tag: Some(tt.to_string()),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// A BYE on the dialog the 200 confirmed is the obligation met.
    #[test]
    fn a_bye_on_a_confirmed_dialog_is_compliant() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        ];
        let f = eval(&UnknownDialog481, &msgs);
        assert_eq!(f.len(), 1, "one occasion, at the endpoint that took it: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, BOB, "the UAS that owed the 481 is charged");
        assert_eq!(f[0].taker, ALICE);
    }

    /// A request naming a peer tag the taker never confirmed is the §12.2.2
    /// case, and serving it is the miss.
    #[test]
    fn a_request_for_an_unknown_peer_tag_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
            rsp(4_000, 200, 2, "BYE", "z9hG4bK-b", ""),
        ];
        let f = hits(&UnknownDialog481, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::UnknownDialogRequest {
            method,
            from_tag,
            answered_status,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((method.as_str(), from_tag.as_str(), *answered_status), ("BYE", "zz", 200));
    }

    /// The 481 discharges it.
    #[test]
    fn a_481_answers_the_unknown_dialog() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
            rsp(4_000, 481, 2, "BYE", "z9hG4bK-b", ""),
        ];
        assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
    }

    /// Without a dialog-creating response on the call, nothing at this vantage
    /// says which dialogs the taker has — an orphan request is no occasion.
    #[test]
    fn an_orphan_request_on_an_unseen_call_is_not_an_occasion() {
        let msgs = [in_dialog(1_000, "BYE", "z9hG4bK-orphan", 2, "at", "zz")];
        assert!(eval(&UnknownDialog481, &msgs).is_empty());
    }

    /// An ACK elicits no response (§17.1.1.3) and a CANCEL is matched by
    /// transaction (§9.1): neither can be answered 481.
    #[test]
    fn ack_and_cancel_are_not_occasions() {
        for method in ["ACK", "CANCEL"] {
            let msgs = [
                invite(1_000, "z9hG4bK-i", ""),
                rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
                in_dialog(3_000, method, "z9hG4bK-x", 1, "zz", "bt"),
            ];
            assert!(eval(&UnknownDialog481, &msgs).is_empty(), "{method}");
        }
    }

    /// One relay face carries BOTH directions of a call, so it knows both of
    /// its peers: the peer-tag key is what keeps it uncharged.
    #[test]
    fn a_face_that_carried_both_directions_knows_both_peers() {
        const PROXY: &str = "127.0.0.1:5080";
        let mut took_invite = invite(1_000, "z9hG4bK-i", "");
        took_invite.dst = PROXY.to_string();
        let mut passed_200 = rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "");
        passed_200.src = PROXY.to_string();
        let mut took_bye = in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
        took_bye.dst = PROXY.to_string();
        let msgs = [took_invite, passed_200, took_bye];
        assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
    }

    /// An unanswered request inside an OPEN observation's window is truncation,
    /// not silence.
    #[test]
    fn an_unanswered_request_inside_the_window_settles_nothing() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
        ];
        let open = Observation { last_us: 3_500, closed: false, ..Observation::default() };
        let f = UnknownDialog481.eval(&WireView { msgs: &msgs, obs: &open });
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);
        assert_eq!(hits(&UnknownDialog481, &msgs).len(), 1, "closed, it decides at once");
    }

    // ── NoByeOutsideOrEarlyDialog (RFC 3261 §15) ────────────────────────────

    /// A request bob sent alice, with caller-controlled tags — the
    /// callee-initiated orientation, where From/To are reversed relative to the
    /// establishing INVITE.
    fn from_bob(at_us: u64, method: &str, branch: &str, cseq: u32, ft: &str, tt: &str) -> Msg {
        let head = format!(
            "{method} {A_URI} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <{B_URI}>;tag={ft}\r\n\
             To: <{A_URI}>;tag={tt}\r\n\
             Call-ID: c1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        );
        Msg {
            at_us,
            src: BOB.to_string(),
            dst: ALICE.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: method.to_string(),
            from_tag: Some(ft.to_string()),
            to_tag: (!tt.is_empty()).then(|| tt.to_string()),
            via_branch: Some(branch.to_string()),
            head: Some(head.into_bytes()),
            body: None,
        }
    }

    /// The caller BYE-ing a dialog its own 2xx confirmed: the ordinary
    /// teardown, and one occasion.
    #[test]
    fn a_caller_bye_on_a_confirmed_dialog_is_compliant() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        ];
        let f = eval(&NoByeOutsideOrEarlyDialog, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::NoByeOutsideOrEarlyDialog);
        assert_eq!(f[0].emitter, ALICE, "the BYE's sender is charged");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// A BYE carrying no peer tag names no dialog at all.
    #[test]
    fn a_bye_with_no_peer_tag_is_violated() {
        let mut bye = in_dialog(1_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
        bye.to_tag = None;
        let f = hits(&NoByeOutsideOrEarlyDialog, &[bye]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::ByeOffDialog { early_dialog, to_tag, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert!(!early_dialog, "the no-dialog shape, not the early one");
        assert!(to_tag.is_empty());
    }

    /// The callee BYE-ing a dialog it answered 180 and never accepted: §15 has
    /// it reject the pending INVITE instead, so the caller's transaction still
    /// gets a final.
    #[test]
    fn a_callee_bye_on_an_early_dialog_is_violated() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
            from_bob(3_000, "BYE", "z9hG4bK-b", 2, "bt", "at"),
        ];
        let f = hits(&NoByeOutsideOrEarlyDialog, &msgs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, BOB, "the callee is charged");
        let Decision::Violated(Evidence::ByeOffDialog { early_dialog, .. }) = &f[0].decision else {
            panic!("{:?}", f[0].decision)
        };
        assert!(early_dialog);
    }

    /// Once the callee has accepted, its own BYE is the ordinary teardown —
    /// tags reversed and all, because the dialog is keyed by the PEER's tag.
    #[test]
    fn a_callee_bye_after_its_own_2xx_is_compliant() {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
            rsp(3_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            from_bob(4_000, "BYE", "z9hG4bK-b", 2, "bt", "at"),
        ];
        assert!(
            hits(&NoByeOutsideOrEarlyDialog, &msgs).is_empty(),
            "{:?}",
            eval(&NoByeOutsideOrEarlyDialog, &msgs)
        );
    }

    /// A UAC answering its PEER's later re-INVITE has answered an INVITE, but
    /// not THE INVITE: it is nobody's callee, so its own BYE is clean. Reading
    /// "is the callee" off the establishing INVITE's branch is what keeps it so.
    #[test]
    fn a_caller_that_rejected_a_reverse_re_invite_may_still_bye() {
        // Alice establishes, bob re-INVITEs from his side, alice rejects it 500,
        // then alice tears the (long-confirmed) dialog down.
        let mut reverse_500 = rsp_tagged(4_000, 500, 1, "INVITE", "z9hG4bK-r", Some("at"), "");
        reverse_500.src = ALICE.to_string();
        reverse_500.dst = BOB.to_string();
        reverse_500.from_tag = Some("bt".to_string());
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            from_bob(3_000, "INVITE", "z9hG4bK-r", 1, "bt", "at"),
            reverse_500,
            in_dialog(5_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        ];
        assert!(
            hits(&NoByeOutsideOrEarlyDialog, &msgs).is_empty(),
            "{:?}",
            eval(&NoByeOutsideOrEarlyDialog, &msgs)
        );
    }

    /// A retransmitted BYE is the same act again: one occasion, not two.
    #[test]
    fn a_retransmitted_bye_is_one_occasion() {
        let mut again = in_dialog(4_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
        again.repeat = true;
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
            again,
        ];
        assert_eq!(eval(&NoByeOutsideOrEarlyDialog, &msgs).len(), 1);
    }
}
