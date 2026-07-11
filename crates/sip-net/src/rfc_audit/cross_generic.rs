//! Port of `tests/harness/rules/rfc/cross-message-rules.ts` — the generic
//! per-dialog cross-message rules.
//!
//! Authoring pattern (copy this for the remaining cross rules): a unit struct
//! implementing [`CrossMessageAuditRule`], project the channel with
//! [`project_per_dialog`], then for each agent slot walk its ordered stream
//! feeding every event through [`advance_dialog_model`] (before *and* after the
//! per-message check, exactly as the TS `advanceDialogModel` placement) and push
//! `(bind_key, detail)` findings. Add the struct to [`cross_rules`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    advance_dialog_model, call_id, cseq_method, cseq_seq, extract_route_uri, from_uri,
    is_in_dialog_request, msg_headers, parse_sdp_origin, project_per_dialog, route_is_loose,
    slot_is_relay, status, to_tag, to_uri, top_via_branch, DialogModel, EventKind, OrderedEvent,
    ParsedSdpOrigin,
};
use crate::types::UaRole;
use sip_message::message_helpers::{get_header, get_headers, parse_sip_uri};
use sip_message::SipMessage;

/// Body bytes of a message (empty for a bodyless message). Mirrors the TS
/// `msg.body` accessor used by the SDP-origin continuity rule.
fn body_of(m: &SipMessage) -> &[u8] {
    match m {
        SipMessage::Request(r) => &r.body,
        SipMessage::Response(r) => &r.body,
    }
}

/// All values of `name` on a message, in wire order (TS `getAllHeaderValues`).
fn all_header_values<'a>(m: &'a SipMessage, name: &str) -> Vec<&'a str> {
    get_headers(msg_headers(m), name)
}

/// First value of `name` on a message, if present (TS `getHeaderValue`). `None`
/// distinguishes "header absent" from "header present, empty value".
fn header_value<'a>(m: &'a SipMessage, name: &str) -> Option<&'a str> {
    get_header(msg_headers(m), name)
}

/// The `rport` parameter on the **top** Via header of `m`, mirroring the TS
/// `readRport`. Returns `(present, value)`: `present` is whether `rport` appears
/// at all; `value` is the numeric port iff it carries one (`rport=N`), `None`
/// for the bare flag (`;rport`).
fn read_top_via_rport(m: &SipMessage) -> (bool, Option<i64>) {
    let Some(via) = get_headers(msg_headers(m), "via").into_iter().next() else {
        return (false, None);
    };
    for piece in via.split(';').skip(1) {
        let piece = piece.trim();
        let (key, val) = match piece.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (piece, None),
        };
        if key.eq_ignore_ascii_case("rport") {
            return match val {
                None => (true, None),
                Some(v) => (true, v.parse::<i64>().ok()),
            };
        }
    }
    (false, None)
}

/// **RFC 3261 §12.2.1.1 — in-dialog request URIs are stable.** Once a dialog is
/// established, the From/To URIs of every in-dialog request the UAC sends MUST
/// match the dialog's local/remote URIs (the From/To learned when the dialog was
/// created). A B2BUA that rewrites the From or To URI on a re-INVITE/UPDATE/BYE
/// mid-dialog breaks the peer's dialog matching — a real UAS would 481 it, the
/// test UA answers it.
pub struct MidDialogUriRule;

impl CrossMessageAuditRule for MidDialogUriRule {
    fn name(&self) -> &'static str {
        "rfc3261.midDialogUri"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                // A transparent relay (proxy) carries the originator's From/To
                // through unchanged — per-UA URI stability does not apply to it.
                if slot_is_relay(slot) {
                    continue;
                }
                let mut m = DialogModel::empty();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            let method = req.method.as_str();
                            // The initial INVITE (and CANCEL, which is hop-by-hop)
                            // establish/precede the dialog — not in-dialog requests.
                            let is_initial_invite = method == "INVITE"
                                && m.initial_invite_sent_branch.is_empty()
                                && m.initial_invite_received_branch.is_empty();
                            if method != "CANCEL"
                                && !is_initial_invite
                                && is_in_dialog_request(req, &m)
                            {
                                if !m.dialog_local_uri.is_empty()
                                    && from_uri(&ev.msg) != m.dialog_local_uri
                                {
                                    out.push((
                                        slot.bind_key.clone(),
                                        format!(
                                            "in-dialog {method} From URI \"{}\" differs from dialog \
                                             local URI \"{}\" — RFC 3261 §12.2.1.1",
                                            from_uri(&ev.msg),
                                            m.dialog_local_uri,
                                        ),
                                    ));
                                }
                                if !m.dialog_remote_uri.is_empty()
                                    && to_uri(&ev.msg) != m.dialog_remote_uri
                                {
                                    out.push((
                                        slot.bind_key.clone(),
                                        format!(
                                            "in-dialog {method} To URI \"{}\" differs from dialog \
                                             remote URI \"{}\" — RFC 3261 §12.2.1.1",
                                            to_uri(&ev.msg),
                                            m.dialog_remote_uri,
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                    advance_dialog_model(&mut m, ev);
                }
            }
        }
        out
    }
}

/// **RFC 3261 §12.2.1.1 / §16.12 — in-dialog requests honour the dialog route
/// set.** Once a dialog's route set is fixed (the Record-Route stack reversed at
/// establishment), every in-dialog request the UAC sends MUST reproduce it: with
/// a *loose* first route the request carries the route set verbatim as Route
/// headers; with a *strict* first route the Request-URI becomes the first route
/// URI and the remaining routes (optionally with the target appended) ride the
/// Route headers. A B2BUA that drops or reorders the route set mid-dialog
/// mis-routes the request through the proxies that record-routed — a real proxy
/// stack would lose the dialog, the test UA forwards it regardless.
pub struct MidDialogRouteRule;

impl CrossMessageAuditRule for MidDialogRouteRule {
    fn name(&self) -> &'static str {
        "rfc3261.midDialogRoute"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let mut m = DialogModel::empty();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            let method = req.method.as_str();
                            let is_initial_invite = method == "INVITE"
                                && m.initial_invite_sent_branch.is_empty()
                                && m.initial_invite_received_branch.is_empty();
                            // CANCEL/ACK are §9.1/§17 hop-by-hop — not governed
                            // by the dialog route set the way other in-dialog
                            // requests are. Only judge in-dialog requests once
                            // the route set is non-empty.
                            if method != "CANCEL"
                                && method != "ACK"
                                && !is_initial_invite
                                && is_in_dialog_request(req, &m)
                                && !m.route_set.is_empty()
                            {
                                check_mid_dialog_route(&mut out, slot, &m, &ev.msg, method);
                            }
                        }
                    }
                    advance_dialog_model(&mut m, ev);
                }
            }
        }
        out
    }
}

/// The routing-significant `host:port` of a Route/Record-Route URI (params
/// stripped). Used to compare a reproduced Route against the dialog route set
/// without tripping on proxy-rewritten per-direction parameters.
fn route_host_port(uri: &str) -> String {
    sip_message::message_helpers::extract_host_port(uri)
        .map(|(h, p)| format!("{h}:{p}"))
        .unwrap_or_else(|| uri.to_string())
}

/// The per-message Route check, factored out of [`MidDialogRouteRule`] to keep
/// the slot walk readable (mirrors the TS inline block).
fn check_mid_dialog_route(
    out: &mut Vec<(LaneKey, String)>,
    slot: &crate::rfc_audit::dialog_model::AgentSlot,
    m: &DialogModel,
    msg: &SipMessage,
    method: &str,
) {
    // Split comma-folded Route headers so the count/sequence compares like-for-like
    // with the (also comma-split) dialog route set — RFC 3261 §7.3.1.
    let sent_routes = crate::rfc_audit::dialog_model::split_header_values(msg, "route");
    let first_route = &m.route_set[0];

    if route_is_loose(first_route) {
        if sent_routes.is_empty() {
            out.push((
                slot.bind_key.clone(),
                format!(
                    "in-dialog {method} omits Route header although the dialog route set is \
                     non-empty (loose, first entry \"{first_route}\") — RFC 3261 §12.2.1.1"
                ),
            ));
        } else if sent_routes.len() != m.route_set.len() {
            out.push((
                slot.bind_key.clone(),
                format!(
                    "in-dialog {method} Route header count {} differs from dialog route set \
                     length {} — RFC 3261 §12.2.1.1",
                    sent_routes.len(),
                    m.route_set.len(),
                ),
            ));
        } else {
            for (i, sent) in sent_routes.iter().enumerate() {
                let expected = extract_route_uri(&m.route_set[i]);
                let actual = extract_route_uri(sent);
                // Compare the routing-significant host:port, NOT the full URI: a
                // record-routing proxy legitimately rewrites Record-Route URI
                // PARAMETERS per direction (a signed stateful cookie `;e=;kid=;sig=`
                // toward one leg, a bare `;outbound`/`;target=` toward the other),
                // so requiring verbatim param equality would false-positive on
                // every stateful proxy. The §12.2.1.1 invariant the audit must
                // enforce is that the route set is reproduced in order to the same
                // hops — drops/reorders/wrong-host — which host:port captures.
                if route_host_port(&expected) != route_host_port(&actual) {
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "in-dialog {method} Route[{i}] \"{actual}\" routes to a different \
                             host:port than dialog route set entry \"{expected}\" — RFC 3261 \
                             §12.2.1.1"
                        ),
                    ));
                    break;
                }
            }
        }
    } else {
        let expected_uri = extract_route_uri(first_route);
        let req_uri = match msg {
            SipMessage::Request(r) => r.uri.as_str(),
            SipMessage::Response(_) => "",
        };
        if req_uri != expected_uri {
            out.push((
                slot.bind_key.clone(),
                format!(
                    "in-dialog {method} Request-URI \"{req_uri}\" should be first strict route \
                     URI \"{expected_uri}\" — RFC 3261 §16.12"
                ),
            ));
        }
        let expected_tail: Vec<String> =
            m.route_set[1..].iter().map(|r| extract_route_uri(r)).collect();
        let actual_tail: Vec<String> =
            sent_routes.iter().map(|r| extract_route_uri(r)).collect();
        let matches_exact = expected_tail == actual_tail;
        let matches_with_target = actual_tail.len() == expected_tail.len() + 1
            && expected_tail
                .iter()
                .zip(actual_tail.iter())
                .all(|(e, a)| e == a);
        if !matches_exact && !matches_with_target {
            out.push((
                slot.bind_key.clone(),
                format!(
                    "in-dialog {method} strict-route Route tail does not match dialog route set \
                     (expected {expected_tail:?}, got {actual_tail:?}) — RFC 3261 §16.12"
                ),
            ));
        }
    }
}

/// **RFC 3261 §8.1.2 + RFC 3263 §4 — in-dialog requests go to the route-derived
/// wire destination.** With a non-empty loose route set §12.2.1.1 puts the next
/// hop's URI as the topmost Route; RFC 3263 §4 resolves it to a `(host, port)`.
/// With an empty route set the request goes to the Request-URI. This rule
/// confirms the bytes were `send`'d to that derived destination. It stays silent
/// before a dialog is confirmed (no remote tag) — there §8.1.2 permits a
/// configured outbound proxy. A B2BUA pinned to an outbound proxy that decouples
/// wire-dst from the Route URI is the real divergence this catches; a real UA's
/// stack always honours the derivation.
pub struct MidDialogWireDestinationRule;

impl CrossMessageAuditRule for MidDialogWireDestinationRule {
    fn name(&self) -> &'static str {
        "rfc3261.midDialogWireDestination"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let mut m = DialogModel::empty();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            let method = req.method.as_str();
                            let is_initial_invite = method == "INVITE"
                                && m.initial_invite_sent_branch.is_empty()
                                && m.initial_invite_received_branch.is_empty();
                            // CANCEL/ACK (§9.1/§17) follow the INVITE's
                            // destination, not the route set; an unconfirmed
                            // dialog (no remote tag) may legitimately use an
                            // outbound proxy.
                            if method != "CANCEL"
                                && method != "ACK"
                                && !is_initial_invite
                                && !m.remote_tag.is_empty()
                            {
                                check_wire_destination(&mut out, slot, &ev.msg, method, ev);
                            }
                        }
                    }
                    advance_dialog_model(&mut m, ev);
                }
            }
        }
        out
    }
}

/// The per-message wire-destination check, factored out of
/// [`MidDialogWireDestinationRule`].
fn check_wire_destination(
    out: &mut Vec<(LaneKey, String)>,
    slot: &crate::rfc_audit::dialog_model::AgentSlot,
    msg: &SipMessage,
    method: &str,
    ev: &OrderedEvent,
) {
    let sent_routes = all_header_values(msg, "route");
    let target_uri = if let Some(first) = sent_routes.first() {
        extract_route_uri(first)
    } else {
        match msg {
            SipMessage::Request(r) => r.uri.clone(),
            SipMessage::Response(_) => return,
        }
    };
    let Some(parsed) = parse_sip_uri(&target_uri) else {
        return;
    };
    // Unit-test fixtures may omit wire info — skip rather than false-fire.
    let Some(peer) = ev.wire_peer else {
        return;
    };
    let peer_ip = peer.ip().to_string();
    let peer_port = u64::from(peer.port());
    if peer_ip != parsed.host || peer_port != parsed.port {
        let lead = if sent_routes.is_empty() {
            "Request-URI resolves to "
        } else {
            "topmost Route URI resolves to "
        };
        out.push((
            slot.bind_key.clone(),
            format!(
                "in-dialog {method} wire-sent to {peer_ip}:{peer_port} but {lead}{}:{} \
                 (\"{target_uri}\") — RFC 3261 §8.1.2 + RFC 3263 §4",
                parsed.host, parsed.port,
            ),
        ));
    }
}

/// **RFC 4566 §5.2 / RFC 3264 §8 — SDP `o=` line is a stable, monotonic session
/// identifier.** Within one session a UA's successive SDP bodies MUST keep the
/// `o=` tuple (username / session-id / network / address) fixed; the
/// session-version increments by exactly one when (and only when) the rest of the
/// SDP changes, and never goes backwards.
///
/// **Advisory** (TS `severityOverride: "advisory"`): "B2BUA-mediated transfer
/// fixtures emit fresh SDP from each side without preserving the originator's
/// `o=` tuple; per-fixture allowance until Phase 2 narrows subject or models
/// 3264 §8 origin replication." A real endpoint mints one origin per session; a
/// B2BUA re-offers with its own origin, which this rule flags but cannot gate on.
///
/// Relay slots are skipped and the subject is `{Uac, Uas}`: a transparent proxy
/// forwards BOTH directions' SDP on one bind, so its "sent" stream legitimately
/// interleaves alice's `o=alice` offer and bob's `o=bob` answer — origin
/// continuity is a per-ORIGINATOR invariant and judging a relay's mixed stream
/// against it false-fired on every proxy lane (the e2e LB class).
pub struct SdpOriginContinuityRule;

#[derive(Clone)]
struct OriginHistory {
    origin: ParsedSdpOrigin,
    raw_digest: String,
}

impl CrossMessageAuditRule for SdpOriginContinuityRule {
    fn name(&self) -> &'static str {
        "rfc3264.sdpOriginContinuity"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uac, UaRole::Uas])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        // Per-(bindKey, callId) history of this agent's sent SDP origin. Forked
        // early dialogs share Call-ID + From-tag and observe the same UAC
        // emissions, so we key on bind + Call-ID rather than per-slice.
        let mut history: HashMap<String, OriginHistory> = HashMap::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let body = body_of(&ev.msg);
                    if body.is_empty() {
                        continue;
                    }
                    let Some(origin) = parse_sdp_origin(body) else {
                        continue;
                    };
                    let cid = call_id(&ev.msg);
                    if cid.is_empty() {
                        continue;
                    }
                    let key = format!("{}\x00{cid}", slot.bind_key);
                    let Some(prior) = history.get(&key).cloned() else {
                        history.insert(
                            key,
                            OriginHistory {
                                raw_digest: origin.body_digest_excluding_origin.clone(),
                                origin,
                            },
                        );
                        continue;
                    };
                    let tuple_stable = prior.origin.username == origin.username
                        && prior.origin.session_id == origin.session_id
                        && prior.origin.nettype == origin.nettype
                        && prior.origin.addrtype == origin.addrtype
                        && prior.origin.unicast_address == origin.unicast_address;
                    if !tuple_stable {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "SDP origin tuple changed within session — prior \"{}\", new \
                                 \"{}\" — RFC 4566 §5.2 / RFC 3264 §8",
                                prior.origin.raw_origin_line, origin.raw_origin_line,
                            ),
                        ));
                        history.insert(
                            key,
                            OriginHistory {
                                raw_digest: origin.body_digest_excluding_origin.clone(),
                                origin,
                            },
                        );
                        continue;
                    }
                    let body_changed =
                        prior.raw_digest != origin.body_digest_excluding_origin;
                    let version_delta = origin.session_version - prior.origin.session_version;
                    if body_changed && version_delta != 1 {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "SDP body changed but sess-version went from {} to {} (expected \
                                 exactly +1) — RFC 3264 §8",
                                prior.origin.session_version, origin.session_version,
                            ),
                        ));
                    } else if !body_changed && version_delta != 0 {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "SDP body unchanged but sess-version went from {} to {} (expected \
                                 unchanged for byte-identical SDP) — RFC 4566 §5.2",
                                prior.origin.session_version, origin.session_version,
                            ),
                        ));
                    } else if version_delta < 0 {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "SDP sess-version went backwards ({} → {}) — RFC 4566 §5.2",
                                prior.origin.session_version, origin.session_version,
                            ),
                        ));
                    }
                    history.insert(
                        key,
                        OriginHistory {
                            raw_digest: origin.body_digest_excluding_origin.clone(),
                            origin,
                        },
                    );
                }
            }
        }
        out
    }
}

/// **RFC 3261 §12.1.1 / §12.2.2 — Record-Route is dialog-establishment-only.**
/// A 100 Trying is not dialog-creating, so a Record-Route on it is vestigial;
/// and once a dialog exists its route set is fixed, so a response to an
/// *in-dialog* request MUST NOT carry Record-Route (it cannot mutate the route
/// set). A B2BUA/proxy that leaks Record-Route on these responses confuses a
/// strict UAC's route-set bookkeeping; the test UA ignores it.
pub struct RecordRoutePlacementRule;

impl CrossMessageAuditRule for RecordRoutePlacementRule {
    fn name(&self) -> &'static str {
        "rfc3261.recordRoutePlacement"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // Sent requests, keyed by top-Via branch, recording whether the
                // request was in-dialog (To-tag present) when sent.
                let mut sent_by_branch: HashMap<String, (String, bool)> = HashMap::new();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            if let Some(branch) = top_via_branch(&ev.msg) {
                                let has_to_tag = to_tag(&ev.msg).is_some();
                                sent_by_branch.insert(
                                    branch,
                                    (req.method.as_str().to_string(), has_to_tag),
                                );
                            }
                        }
                        continue;
                    }
                    // Received response.
                    let SipMessage::Response(_) = &ev.msg else {
                        continue;
                    };
                    let rr = all_header_values(&ev.msg, "record-route");
                    if rr.is_empty() {
                        continue;
                    }
                    if status(&ev.msg) == 100 {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "100 Trying carries Record-Route header(s) — 100 is not \
                                 dialog-creating, Record-Route is vestigial here per RFC 3261 \
                                 §12.1.1. Found: {}",
                                rr[0]
                            ),
                        ));
                        continue;
                    }
                    let sent = top_via_branch(&ev.msg).and_then(|b| sent_by_branch.get(&b));
                    if let Some((method, true)) = sent {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "{} response to in-dialog {method} carries Record-Route — route \
                                 set is fixed at dialog establishment per RFC 3261 §12.2.2. \
                                 Found: {}",
                                status(&ev.msg),
                                rr[0],
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// **RFC 3581 §4 — a server that receives `rport` MUST echo `rport=<source-port>`.**
/// When a request's top Via advertises a bare `;rport`, the response's top Via
/// MUST carry `rport=N` with the observed source port. Dropping it, or echoing an
/// empty `rport`, breaks symmetric-response routing through NAT.
///
/// **Advisory** (TS `severityOverride: "advisory"`): "B2BUA responses on loopback
/// do not echo rport= because the source-port lookup only triggers under NAT (no
/// NAT on 127.0.0.1). Harmless in fake-stack; Phase 2 will either narrow subject
/// to {proxy} only or model loopback explicitly." A real NAT'd server echoes it;
/// the loopback fake-stack legitimately does not, so this is flagged not gated.
pub struct RportEchoRule;

impl CrossMessageAuditRule for RportEchoRule {
    fn name(&self) -> &'static str {
        "rfc3261.rportEcho"
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // Branches on which this agent's SENT request advertised a bare
                // `;rport` (present, no value).
                let mut sent_rport_by_branch: HashMap<String, String> = HashMap::new();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            let (present, value) = read_top_via_rport(&ev.msg);
                            if present && value.is_none() {
                                if let Some(branch) = top_via_branch(&ev.msg) {
                                    sent_rport_by_branch
                                        .insert(branch, req.method.as_str().to_string());
                                }
                            }
                        }
                        continue;
                    }
                    let SipMessage::Response(_) = &ev.msg else {
                        continue;
                    };
                    let Some(branch) = top_via_branch(&ev.msg) else {
                        continue;
                    };
                    let Some(method) = sent_rport_by_branch.get(&branch) else {
                        continue;
                    };
                    let (present, value) = read_top_via_rport(&ev.msg);
                    if !present {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "response {} to {method} (branch {branch}) dropped the rport \
                                 parameter the request advertised — RFC 3581 §4 requires the \
                                 server to echo rport=<source-port>",
                                status(&ev.msg),
                            ),
                        ));
                        continue;
                    }
                    if value.is_none() {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "response {} to {method} (branch {branch}) keeps an empty rport \
                                 parameter — RFC 3581 §4 requires the server to set it to the \
                                 source port",
                                status(&ev.msg),
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// **RFC 3261 §13.2.1 / §20.37 — re-INVITEs and 2xx INVITE answers SHOULD carry
/// Allow and Supported.** A received re-INVITE and a received 2xx to an INVITE
/// should advertise the methods (Allow) and extensions (Supported) the sender
/// accepts so the peer can negotiate Require. A B2BUA that strips these on
/// re-offers hides its capability set; the test UA never inspects them.
pub struct AllowSupportedOnInviteRule;

impl CrossMessageAuditRule for AllowSupportedOnInviteRule {
    fn name(&self) -> &'static str {
        "rfc3261.allowSupportedOnInvite"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // First INVITE branch seen per Call-ID — the initial INVITE is
                // exempt (only re-INVITEs are judged).
                let mut initial_invite_branch_by_call_id: HashMap<String, String> = HashMap::new();
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Received {
                        continue;
                    }
                    match &ev.msg {
                        SipMessage::Request(req) if req.method.as_str() == "INVITE" => {
                            let cid = call_id(&ev.msg).to_string();
                            let branch = top_via_branch(&ev.msg).unwrap_or_default();
                            match initial_invite_branch_by_call_id.entry(cid) {
                                std::collections::hash_map::Entry::Vacant(e) => {
                                    e.insert(branch);
                                    continue;
                                }
                                std::collections::hash_map::Entry::Occupied(e) => {
                                    // A same-branch repeat is a §17.2.3 Timer-A
                                    // RETRANSMISSION of the initial INVITE, not a
                                    // re-INVITE — exempt like the initial itself.
                                    if *e.get() == branch {
                                        continue;
                                    }
                                }
                            }
                            check_allow_supported(&mut out, slot, "re-INVITE", &ev.msg);
                        }
                        SipMessage::Response(resp)
                            if (200..300).contains(&resp.status)
                                && cseq_method(&ev.msg) == "INVITE" =>
                        {
                            let label = format!("{} OK INVITE", resp.status);
                            check_allow_supported(&mut out, slot, &label, &ev.msg);
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

/// Push a finding for each of Allow / Supported missing on `msg` (TS
/// `checkAllowSupported`). Allow absent uses §13.2.1; Supported absent §20.37.
fn check_allow_supported(
    out: &mut Vec<(LaneKey, String)>,
    slot: &crate::rfc_audit::dialog_model::AgentSlot,
    label: &str,
    msg: &SipMessage,
) {
    if header_value(msg, "allow").is_none() {
        out.push((
            slot.bind_key.clone(),
            format!(
                "{label} missing Allow: header — RFC 3261 §13.2.1 (SHOULD list accepted methods)"
            ),
        ));
    }
    if header_value(msg, "supported").is_none() {
        out.push((
            slot.bind_key.clone(),
            format!(
                "{label} missing Supported: header — RFC 3261 §20.37 (SHOULD list extensions \
                 for Require negotiation)"
            ),
        ));
    }
}

/// **RFC 3261 §16.7 step 5 — a stateful proxy absorbs the downstream 100 Trying.**
/// A proxy generates its own 100 toward the UAC and MUST NOT forward the 100 the
/// next hop emits, so the UAC should observe at most one 100 per INVITE
/// transaction (Call-ID + CSeq). A second received 100 means a proxy on the path
/// forwarded one it should have absorbed; the test UAC silently ignores the
/// duplicate.
pub struct Proxy100TryingNotForwardedRule;

impl CrossMessageAuditRule for Proxy100TryingNotForwardedRule {
    fn name(&self) -> &'static str {
        "rfc3261.proxy100TryingNotForwarded"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // INVITE transactions this agent originated, keyed Call-ID|CSeq.
                let mut sent_invite_key: HashSet<String> = HashSet::new();
                let mut trying_count_by_key: HashMap<String, u32> = HashMap::new();
                for ev in &slot.ordered {
                    if ev.kind == EventKind::Sent {
                        if let SipMessage::Request(req) = &ev.msg {
                            if req.method.as_str() == "INVITE" {
                                let key =
                                    format!("{}|{}", call_id(&ev.msg), cseq_seq(&ev.msg));
                                sent_invite_key.insert(key);
                            }
                        }
                        continue;
                    }
                    // Received 100 to an INVITE.
                    if let SipMessage::Response(_) = &ev.msg {
                        if status(&ev.msg) == 100 && cseq_method(&ev.msg) == "INVITE" {
                            let cid = call_id(&ev.msg).to_string();
                            let cseq = cseq_seq(&ev.msg);
                            let key = format!("{cid}|{cseq}");
                            if !sent_invite_key.contains(&key) {
                                continue;
                            }
                            let next = trying_count_by_key.entry(key).or_insert(0);
                            *next += 1;
                            if *next == 2 {
                                out.push((
                                    slot.bind_key.clone(),
                                    format!(
                                        "received a second 100 Trying for INVITE CSeq={cseq} \
                                         Call-ID={cid} — the stateful proxy on the path forwarded \
                                         a downstream 100 it should have absorbed per RFC 3261 \
                                         §16.7 step 5"
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(MidDialogUriRule),
        Arc::new(MidDialogRouteRule),
        Arc::new(MidDialogWireDestinationRule),
        Arc::new(SdpOriginContinuityRule),
        Arc::new(RecordRoutePlacementRule),
        Arc::new(RportEchoRule),
        Arc::new(AllowSupportedOnInviteRule),
        Arc::new(Proxy100TryingNotForwardedRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    fn sent(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: to.parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(bind: &str, raw: Vec<u8>, src: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket { raw, src: src.parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    // Alice's INVITE then in-dialog BYE. `from_uri`/`to_uri` control whether the
    // BYE keeps the dialog URIs (clean) or mutates them (flagged).
    fn invite(branch: &str) -> Vec<u8> {
        b2b_msg("INVITE", branch, 1, "sip:alice@127.0.0.1", "sip:bob@127.0.0.1", "at", None)
    }
    fn ok_180() -> Vec<u8> {
        // 180 from bob carrying a To-tag (confirms remote tag) — as received by alice.
        resp(180, 1, "INVITE", "sip:alice@127.0.0.1", "sip:bob@127.0.0.1", "at", "bt")
    }
    fn ok_200() -> Vec<u8> {
        resp(200, 1, "INVITE", "sip:alice@127.0.0.1", "sip:bob@127.0.0.1", "at", "bt")
    }
    fn bye(from: &str, to: &str) -> Vec<u8> {
        b2b_msg("BYE", "z9hG4bK-bye", 2, from, to, "at", Some("bt"))
    }

    fn b2b_msg(
        method: &str,
        branch: &str,
        cseq: u32,
        from_uri: &str,
        to_uri: &str,
        ftag: &str,
        ttag: Option<&str>,
    ) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<{to_uri}>;tag={t}"),
            None => format!("<{to_uri}>"),
        };
        format!(
            "{method} {to_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{from_uri}>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(
        status: u16,
        cseq: u32,
        method: &str,
        from_uri: &str,
        to_uri: &str,
        ftag: &str,
        ttag: &str,
    ) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
             From: <{from_uri}>;tag={ftag}\r\n\
             To: <{to_uri}>;tag={ttag}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn stable_in_dialog_uri_is_clean() {
        let evs = vec![
            sent("alice", invite("z9hG4bK-i"), "127.0.0.1:5070", 0),
            recv("alice", ok_180(), "127.0.0.1:5070", 1),
            recv("alice", ok_200(), "127.0.0.1:5070", 2),
            sent("alice", bye("sip:alice@127.0.0.1", "sip:bob@127.0.0.1"), "127.0.0.1:5070", 3),
        ];
        assert!(MidDialogUriRule.check(&evs).is_empty());
    }

    #[test]
    fn mutated_in_dialog_from_uri_is_flagged() {
        let evs = vec![
            sent("alice", invite("z9hG4bK-i"), "127.0.0.1:5070", 0),
            recv("alice", ok_180(), "127.0.0.1:5070", 1),
            recv("alice", ok_200(), "127.0.0.1:5070", 2),
            // BYE rewrites the From URI mid-dialog.
            sent("alice", bye("sip:eve@127.0.0.1", "sip:bob@127.0.0.1"), "127.0.0.1:5070", 3),
        ];
        let f = MidDialogUriRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("From URI"), "{}", f[0].1);
    }

    // -- shared extended builders --------------------------------------------

    /// An INVITE / in-dialog request with optional extra headers (Route, etc.)
    /// and an optional SDP body.
    #[allow(clippy::too_many_arguments)]
    fn req_full(
        method: &str,
        uri: &str,
        branch: &str,
        cseq: u32,
        ftag: &str,
        ttag: Option<&str>,
        extra: &str,
        body: &str,
    ) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        format!(
            "{method} {uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A response with optional extra headers (Record-Route, Allow, …) and body.
    #[allow(clippy::too_many_arguments)]
    fn resp_full(
        status: u16,
        cseq: u32,
        method: &str,
        branch: &str,
        ttag: &str,
        extra: &str,
        body: &str,
    ) -> Vec<u8> {
        // An empty `ttag` models a tagless To (e.g. 100 Trying) — emit a bare To
        // with no `;tag=` rather than an empty tag value, which is not a valid
        // wire token and the (even lenient) parser rejects ("Empty To tag").
        let to = if ttag.is_empty() {
            "<sip:bob@127.0.0.1>".to_string()
        } else {
            format!("<sip:bob@127.0.0.1>;tag={ttag}")
        };
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    // -- MidDialogRouteRule --------------------------------------------------
    //
    // The UAC route set is the dialog-creating INVITE 200's Record-Route stack
    // reversed. Two RR `p1, p2` ⇒ route set `[p2, p1]` (p2 first). A clean
    // in-dialog BYE replays both as Route headers in that order; dropping them
    // flags.

    fn invite_no_route() -> Vec<u8> {
        req_full("INVITE", "sip:bob@127.0.0.1", "z9hG4bK-i", 1, "at", None, "", "")
    }
    fn ok200_with_rr() -> Vec<u8> {
        resp_full(
            200,
            1,
            "INVITE",
            "z9hG4bK-i",
            "bt",
            "Record-Route: <sip:p1@127.0.0.1;lr>\r\n\
             Record-Route: <sip:p2@127.0.0.1;lr>\r\n",
            "",
        )
    }

    #[test]
    fn in_dialog_route_replay_is_clean() {
        let bye = req_full(
            "BYE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-bye",
            2,
            "at",
            Some("bt"),
            "Route: <sip:p2@127.0.0.1;lr>\r\n\
             Route: <sip:p1@127.0.0.1;lr>\r\n",
            "",
        );
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", ok200_with_rr(), "127.0.0.1:5070", 1),
            sent("alice", bye, "127.0.0.1:5070", 2),
        ];
        assert!(MidDialogRouteRule.check(&evs).is_empty());
    }

    #[test]
    fn in_dialog_route_omitted_is_flagged() {
        // BYE drops the Route headers although the route set is non-empty.
        let bye =
            req_full("BYE", "sip:bob@127.0.0.1", "z9hG4bK-bye", 2, "at", Some("bt"), "", "");
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", ok200_with_rr(), "127.0.0.1:5070", 1),
            sent("alice", bye, "127.0.0.1:5070", 2),
        ];
        let f = MidDialogRouteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("omits Route header"), "{}", f[0].1);
    }

    // -- MidDialogWireDestinationRule ----------------------------------------

    #[test]
    fn wire_dst_matches_route_uri_is_clean() {
        // Empty route set ⇒ expected dst is the Request-URI (bob@127.0.0.1:5070).
        let bye = req_full(
            "BYE",
            "sip:bob@127.0.0.1:5070",
            "z9hG4bK-bye",
            2,
            "at",
            Some("bt"),
            "",
            "",
        );
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", ""), "127.0.0.1:5070", 1),
            sent("alice", bye, "127.0.0.1:5070", 2),
        ];
        assert!(MidDialogWireDestinationRule.check(&evs).is_empty());
    }

    #[test]
    fn wire_dst_mismatch_is_flagged() {
        // Request-URI resolves to :5070 but the bytes went to :9999.
        let bye = req_full(
            "BYE",
            "sip:bob@127.0.0.1:5070",
            "z9hG4bK-bye",
            2,
            "at",
            Some("bt"),
            "",
            "",
        );
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", ""), "127.0.0.1:5070", 1),
            sent("alice", bye, "127.0.0.1:9999", 2),
        ];
        let f = MidDialogWireDestinationRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("wire-sent to 127.0.0.1:9999"), "{}", f[0].1);
    }

    // -- SdpOriginContinuityRule (advisory) ----------------------------------

    fn sdp(origin: &str, extra_media: &str) -> String {
        format!("v=0\r\no={origin}\r\ns=-\r\nt=0 0\r\nm=audio 5000 RTP/AVP 0\r\n{extra_media}")
    }

    #[test]
    fn sdp_origin_stable_version_bump_is_clean() {
        // Offer then re-offer: same o= tuple, body changes, version +1.
        let inv = req_full(
            "INVITE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-i",
            1,
            "at",
            None,
            "",
            &sdp("alice 1 1 IN IP4 10.0.0.1", ""),
        );
        let reinv = req_full(
            "INVITE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-r",
            2,
            "at",
            Some("bt"),
            "",
            &sdp("alice 1 2 IN IP4 10.0.0.1", "a=sendonly\r\n"),
        );
        let evs = vec![
            sent("alice", inv, "127.0.0.1:5070", 0),
            recv("alice", resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", ""), "127.0.0.1:5070", 1),
            sent("alice", reinv, "127.0.0.1:5070", 2),
        ];
        assert!(SdpOriginContinuityRule.check(&evs).is_empty());
    }

    #[test]
    fn sdp_origin_tuple_change_is_flagged() {
        let inv = req_full(
            "INVITE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-i",
            1,
            "at",
            None,
            "",
            &sdp("alice 1 1 IN IP4 10.0.0.1", ""),
        );
        // Re-offer with a different session-id ⇒ tuple changed.
        let reinv = req_full(
            "INVITE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-r",
            2,
            "at",
            Some("bt"),
            "",
            &sdp("alice 999 2 IN IP4 10.0.0.1", "a=sendonly\r\n"),
        );
        let evs = vec![
            sent("alice", inv, "127.0.0.1:5070", 0),
            recv("alice", resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", ""), "127.0.0.1:5070", 1),
            sent("alice", reinv, "127.0.0.1:5070", 2),
        ];
        let f = SdpOriginContinuityRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("origin tuple changed"), "{}", f[0].1);
        assert!(SdpOriginContinuityRule.force_advisory());
    }

    #[test]
    fn sdp_origin_skips_relay_slots() {
        // A transparent proxy forwards alice's offer AND bob's answer on one
        // bind — its sent stream interleaves o=alice / o=bob for one Call-ID.
        // That is not an origin-continuity violation by the relay; the rule
        // must skip the relay slot (it both sent and received the INVITE).
        assert_eq!(
            SdpOriginContinuityRule.subject(),
            std::collections::HashSet::from([UaRole::Uac, UaRole::Uas])
        );
        let inv = req_full(
            "INVITE",
            "sip:bob@127.0.0.1",
            "z9hG4bK-i",
            1,
            "at",
            None,
            "",
            &sdp("alice 1 1 IN IP4 10.0.0.1", ""),
        );
        let ok = resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", &sdp("bob 1 1 IN IP4 10.0.0.2", ""));
        let evs = vec![
            recv("proxy", inv.clone(), "127.0.0.1:5060", 0), // alice → proxy
            sent("proxy", inv, "127.0.0.1:5070", 1),         // proxy → bob (o=alice)
            recv("proxy", ok.clone(), "127.0.0.1:5070", 2),  // bob → proxy
            sent("proxy", ok, "127.0.0.1:5060", 3),          // proxy → alice (o=bob)
        ];
        assert!(
            SdpOriginContinuityRule.check(&evs).is_empty(),
            "{:?}",
            SdpOriginContinuityRule.check(&evs)
        );
    }

    // -- RecordRoutePlacementRule --------------------------------------------

    #[test]
    fn record_route_on_dialog_creating_response_is_clean() {
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", ok200_with_rr(), "127.0.0.1:5070", 1),
        ];
        assert!(RecordRoutePlacementRule.check(&evs).is_empty());
    }

    #[test]
    fn record_route_on_100_trying_is_flagged() {
        let trying = resp_full(
            100,
            1,
            "INVITE",
            "z9hG4bK-i",
            "",
            "Record-Route: <sip:p1@127.0.0.1;lr>\r\n",
            "",
        );
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", trying, "127.0.0.1:5070", 1),
        ];
        let f = RecordRoutePlacementRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("100 Trying carries Record-Route"), "{}", f[0].1);
    }

    // -- RportEchoRule (advisory) --------------------------------------------

    fn req_rport(branch: &str) -> Vec<u8> {
        format!(
            "OPTIONS sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch};rport\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 5 OPTIONS\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }
    fn resp_via_rport(branch: &str, rport: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}{rport}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 5 OPTIONS\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn rport_echoed_with_value_is_clean() {
        let evs = vec![
            sent("alice", req_rport("z9hG4bK-o"), "127.0.0.1:5070", 0),
            recv("alice", resp_via_rport("z9hG4bK-o", ";rport=5060"), "127.0.0.1:5070", 1),
        ];
        assert!(RportEchoRule.check(&evs).is_empty());
    }

    #[test]
    fn rport_dropped_is_flagged() {
        let evs = vec![
            sent("alice", req_rport("z9hG4bK-o"), "127.0.0.1:5070", 0),
            recv("alice", resp_via_rport("z9hG4bK-o", ""), "127.0.0.1:5070", 1),
        ];
        let f = RportEchoRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("dropped the rport"), "{}", f[0].1);
        assert!(RportEchoRule.force_advisory());
    }

    // -- AllowSupportedOnInviteRule ------------------------------------------

    fn recv_reinvite(extra: &str) -> Vec<u8> {
        // Same Call-ID as the initial INVITE but a distinct branch ⇒ re-INVITE.
        req_full("INVITE", "sip:alice@127.0.0.1", "z9hG4bK-re", 2, "bt", Some("at"), extra, "")
    }
    fn recv_initial_invite() -> Vec<u8> {
        req_full("INVITE", "sip:alice@127.0.0.1", "z9hG4bK-i0", 1, "bt", None, "", "")
    }

    #[test]
    fn reinvite_with_allow_supported_is_clean() {
        let evs = vec![
            recv("alice", recv_initial_invite(), "127.0.0.1:5070", 0),
            recv(
                "alice",
                recv_reinvite("Allow: INVITE, ACK, BYE\r\nSupported: 100rel\r\n"),
                "127.0.0.1:5070",
                1,
            ),
        ];
        assert!(AllowSupportedOnInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn reinvite_missing_allow_supported_is_flagged() {
        let evs = vec![
            recv("alice", recv_initial_invite(), "127.0.0.1:5070", 0),
            recv("alice", recv_reinvite(""), "127.0.0.1:5070", 1),
        ];
        let f = AllowSupportedOnInviteRule.check(&evs);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().any(|(_, d)| d.contains("Allow")));
        assert!(f.iter().any(|(_, d)| d.contains("Supported")));
    }

    // -- Proxy100TryingNotForwardedRule --------------------------------------

    fn recv_100() -> Vec<u8> {
        resp_full(100, 1, "INVITE", "z9hG4bK-i", "", "", "")
    }

    #[test]
    fn single_100_trying_is_clean() {
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", recv_100(), "127.0.0.1:5070", 1),
            recv("alice", resp_full(200, 1, "INVITE", "z9hG4bK-i", "bt", "", ""), "127.0.0.1:5070", 2),
        ];
        assert!(Proxy100TryingNotForwardedRule.check(&evs).is_empty());
    }

    #[test]
    fn second_100_trying_is_flagged() {
        let evs = vec![
            sent("alice", invite_no_route(), "127.0.0.1:5070", 0),
            recv("alice", recv_100(), "127.0.0.1:5070", 1),
            recv("alice", recv_100(), "127.0.0.1:5070", 2),
        ];
        let f = Proxy100TryingNotForwardedRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("second 100 Trying"), "{}", f[0].1);
    }
}
