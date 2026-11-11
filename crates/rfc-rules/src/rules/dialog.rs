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

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use super::Obligation;
use message_facts::request_uri_target;
use reading::Reading;

mod message_facts;
mod reading;
mod record_route;
mod unknown_dialog;

pub use record_route::RecordRoutePlacement;
pub use unknown_dialog::{UnknownDialog481, UNKNOWN_DIALOG_WINDOW_US};

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
/// NON-EMPTY route set — an empty set asks for nothing. Charges the sender. A
/// record-routing proxy that forwarded the establishing INVITE owes only the
/// hops recorded beyond it, not its own row nor those upstream.
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
                    .then(|| sent.iter().zip(set.iter()).position(|(a, e)| !a.same_address(e)))
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
            if sent_to == (target.next_hop_host.clone(), target.port) {
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
                target_host: target.next_hop_host,
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

#[cfg(test)]
mod tests;
