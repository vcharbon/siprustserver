//! Port of `tests/harness/rules/rfc/rfc3261-cross-message-rules.ts` — the
//! RFC 3261 cross-message rules whose enforcement spans more than one message.
//!
//! Authoring pattern mirrors [`super::cross_generic`]: a unit struct per rule
//! implementing [`CrossMessageAuditRule`], projecting the recorded channel with
//! [`project_per_dialog`] (per-UA dialog-walking rules — skip relay slots) or
//! indexing it by top-Via branch with [`build_branch_index`] (CANCEL/ACK/PRACK
//! vs INVITE correlation). Each finding carries the `bind_key` the TS attributes
//! it to. Four rules are advisory (`force_advisory() -> true`) for the same
//! B2BUA-architectural reasons the TS marks them `severityOverride: "advisory"`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::get_headers;
use sip_message::SipMessage;

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, extract_route_uri, from_tag, msg_headers, project_per_dialog,
    route_is_loose, slot_is_relay, status, to_tag, to_uri, top_via_branch, EventKind, OrderedEvent,
};
use crate::rfc_audit::txn_correlation::{
    build_branch_index, header_values, split_option_tags, Direction,
};

// ---------------------------------------------------------------------------
// Shared header helpers (ports of the TS module-level helpers)
// ---------------------------------------------------------------------------

/// All values of a (possibly repeated) header on a parsed message — the TS
/// `getAllHeaderValues(msg.headers, name)` over a raw [`SipMessage`].
fn all_header_values<'a>(m: &'a SipMessage, name: &str) -> Vec<&'a str> {
    get_headers(msg_headers(m), name)
}

/// Split comma-separated option-tag values into normalised lower-case tags —
/// the TS `collectOptionTags`. Reuses [`split_option_tags`] (same semantics).
fn collect_option_tags(values: &[&str]) -> Vec<String> {
    split_option_tags(values.iter().copied())
}

/// Top-Via branch of a message, or empty string (the TS
/// `msg.getHeader("via")[0]?.branch ?? ""`).
fn branch_of(m: &SipMessage) -> String {
    top_via_branch(m).unwrap_or_default()
}

/// Methods a modern UA recognises — the TS `RECOGNISED_METHODS` set.
const RECOGNISED_METHODS: &[&str] = &[
    "INVITE", "ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER", "PRACK", "UPDATE", "INFO", "REFER",
    "SUBSCRIBE", "NOTIFY", "MESSAGE", "PUBLISH",
];

/// Option tags a modern UA recognises — the TS `RECOGNISED_OPTION_TAGS` set.
const RECOGNISED_OPTION_TAGS: &[&str] = &[
    "100rel", "timer", "replaces", "gruu", "path", "outbound", "eventlist", "sec-agree",
];

/// Walk one agent slot's ordered (sent + received) timeline, yielding each
/// event's `(kind, &msg)`.
fn slot_events(slot_ordered: &[OrderedEvent]) -> impl Iterator<Item = (EventKind, &SipMessage)> {
    slot_ordered.iter().map(|ev| (ev.kind, &ev.msg))
}

// ---------------------------------------------------------------------------
// rfc3261.unknownDialog481
// ---------------------------------------------------------------------------

/// **RFC 3261 §12.2.2 — an in-dialog request for an unknown dialog MUST get 481.**
/// A UAS receiving an in-dialog request whose dialog identifier (Call-ID +
/// local-tag + remote-tag) matches none of its dialogs MUST respond 481. The
/// projector keys each slice by `(Call-ID, From-tag, To-tag)`, so a confirmed
/// in-dialog request always lands in its dialog's slice by construction — the
/// only observable "unknown dialog" window is a `to_tag = None` slice that
/// nonetheless received a both-tags request the agent never confirmed. A real
/// UAS 481s it; the test UA answers it. Kept as a tripwire for projector-
/// invariant changes (it never fires under normal slicing).
pub struct UnknownDialog481Rule;

impl CrossMessageAuditRule for UnknownDialog481Rule {
    fn name(&self) -> &'static str {
        "rfc3261.unknownDialog481"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            out.extend(self.check_slice(&slice));
        }
        out
    }
}

impl UnknownDialog481Rule {
    /// The per-slice tripwire body, factored out so the (otherwise unreachable
    /// under normal slicing) None-to_tag branch can be exercised directly. A
    /// confirmed (`to_tag.is_some()`) slice already matched its dialog by
    /// construction, so only a pending (None) slice can expose an "unknown
    /// dialog" both-tags request.
    fn check_slice(&self, slice: &crate::rfc_audit::dialog_model::DialogSlice) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        if slice.to_tag.is_some() {
            return out;
        }
        for slot in &slice.per_agent {
            for (kind, msg) in slot_events(&slot.ordered) {
                if kind != EventKind::Received {
                    continue;
                }
                let SipMessage::Request(req) = msg else {
                    continue;
                };
                let ft = from_tag(msg).unwrap_or("");
                let tt = to_tag(msg).unwrap_or("");
                if ft.is_empty() || tt.is_empty() {
                    continue;
                }
                out.push((
                    slot.bind_key.clone(),
                    format!(
                        "Received in-dialog request {} for unknown dialog {}/{ft}/{tt} — \
                         {{Uas}} must respond 481 (RFC 3261 §12.2.2 / RFC3261-MUST-071)",
                        req.method.as_str(),
                        call_id(msg),
                    ),
                ));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.unsupportedMethod405Allow
// ---------------------------------------------------------------------------

/// **RFC 3261 §8.2.1 — an unrecognised request method MUST get 405 + Allow.**
/// A UAS that does not recognise a request method MUST respond 405 (Method Not
/// Allowed) carrying an Allow header listing the methods it supports. A real UAS
/// rejects the unknown verb; the test UA answers whatever it is handed.
/// Regression-only tripwire — no current fixture emits an unrecognised method.
pub struct UnsupportedMethod405AllowRule;

impl CrossMessageAuditRule for UnsupportedMethod405AllowRule {
    fn name(&self) -> &'static str {
        "rfc3261.unsupportedMethod405Allow"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                // Per-branch: the unrecognised request received, and the
                // response (if any) sent on the same branch.
                let mut unrecognised_by_branch: HashMap<String, (String, String)> = HashMap::new();
                let mut response_by_branch: HashMap<String, (u16, usize)> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Request(req)) => {
                            if RECOGNISED_METHODS
                                .iter()
                                .any(|m| m.eq_ignore_ascii_case(req.method.as_str()))
                            {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            unrecognised_by_branch.entry(branch).or_insert((
                                req.method.as_str().to_string(),
                                call_id(msg).to_string(),
                            ));
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            let branch = branch_of(msg);
                            if branch.is_empty() || !unrecognised_by_branch.contains_key(&branch) {
                                continue;
                            }
                            response_by_branch.entry(branch).or_insert((
                                status(msg),
                                all_header_values(msg, "allow").len(),
                            ));
                        }
                        _ => {}
                    }
                }

                for (branch, (method, cid)) in &unrecognised_by_branch {
                    let resp = response_by_branch.get(branch);
                    let st = resp.map(|r| r.0).unwrap_or(0);
                    let allow_count = resp.map(|r| r.1).unwrap_or(0);
                    if resp.is_some() && st == 405 && allow_count > 0 {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Received unrecognised method {method} (Call-ID {cid}, branch \
                             {branch}) — {{Uas}} must respond 405 with Allow header (RFC 3261 \
                             §8.2.1 / RFC3261-MUST-030); got {st} / Allow={allow_count}"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.unsupportedExtension420
// ---------------------------------------------------------------------------

/// **RFC 3261 §8.2.2 — an unsupported Require tag MUST get 420 + Unsupported.**
/// A UAS receiving a request whose Require header lists an option tag it does not
/// support MUST respond 420 (Bad Extension) carrying an Unsupported header that
/// lists the rejected tags. A real UAS refuses the extension; the test UA
/// ignores Require. Regression-only tripwire.
pub struct UnsupportedExtension420Rule;

impl CrossMessageAuditRule for UnsupportedExtension420Rule {
    fn name(&self) -> &'static str {
        "rfc3261.unsupportedExtension420"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        let recognised: HashSet<&str> = RECOGNISED_OPTION_TAGS.iter().copied().collect();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut unsupported_by_branch: HashMap<String, (String, String, Vec<String>)> =
                    HashMap::new();
                let mut response_by_branch: HashMap<String, (u16, usize)> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Request(req)) => {
                            let require = all_header_values(msg, "require");
                            if require.is_empty() {
                                continue;
                            }
                            let tags = collect_option_tags(&require);
                            let unsupported: Vec<String> = tags
                                .into_iter()
                                .filter(|t| !recognised.contains(t.as_str()))
                                .collect();
                            if unsupported.is_empty() {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            unsupported_by_branch.entry(branch).or_insert((
                                req.method.as_str().to_string(),
                                call_id(msg).to_string(),
                                unsupported,
                            ));
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            let branch = branch_of(msg);
                            if branch.is_empty() || !unsupported_by_branch.contains_key(&branch) {
                                continue;
                            }
                            response_by_branch.entry(branch).or_insert((
                                status(msg),
                                all_header_values(msg, "unsupported").len(),
                            ));
                        }
                        _ => {}
                    }
                }

                for (branch, (method, cid, tags)) in &unsupported_by_branch {
                    let resp = response_by_branch.get(branch);
                    let st = resp.map(|r| r.0).unwrap_or(0);
                    let unsupported_count = resp.map(|r| r.1).unwrap_or(0);
                    if resp.is_some() && st == 420 && unsupported_count > 0 {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Received request {method} requires unsupported option tag(s) [{}] \
                             (callId {cid}, branch {branch}) — {{Uas}} must respond 420 with \
                             Unsupported header; got {st} / Unsupported={unsupported_count}",
                            tags.join(", "),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.unsupported415Accepts
// ---------------------------------------------------------------------------

/// **RFC 3261 §8.2.3 — a 415 response MUST advertise the formats it accepts.**
/// A UAS rejecting an unsupported Content-Type / -Encoding / -Language with 415
/// (Unsupported Media Type) MUST list what it does support via Accept /
/// Accept-Encoding / Accept-Language. A real UAS guides the UAC to a retriable
/// format; the test UA does not. Single-message check, modelled as a cross rule
/// for the per-slot view. Regression-only tripwire.
pub struct Unsupported415AcceptsRule;

impl CrossMessageAuditRule for Unsupported415AcceptsRule {
    fn name(&self) -> &'static str {
        "rfc3261.unsupported415Accepts"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                for (kind, msg) in slot_events(&slot.ordered) {
                    if kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Response(_) = msg else {
                        continue;
                    };
                    if status(msg) != 415 {
                        continue;
                    }
                    let has_accept = !all_header_values(msg, "accept").is_empty()
                        || !all_header_values(msg, "accept-encoding").is_empty()
                        || !all_header_values(msg, "accept-language").is_empty();
                    if has_accept {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent 415 response (callId {}, branch {}) carries no Accept / \
                             Accept-Encoding / Accept-Language header — {{Uas}} RFC 3261 §8.2.3 \
                             / RFC3261-MUST-036",
                            call_id(msg),
                            branch_of(msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.responseExtensionsAdvertised
// ---------------------------------------------------------------------------

/// **RFC 3261 §8.2.2.1 — a UAS MUST NOT apply an unadvertised extension.**
/// Any extension applied to a non-421 response MUST appear in the response's
/// Supported (or Require, if mandatory) header. Narrowly: a sent INVITE response
/// that accepted (1xx/2xx) an INVITE carrying Require tags MUST echo every such
/// tag in Supported/Require — unless it is a 420/421 rejection. A real UAS
/// advertises what it honoured; the test UA stays silent. Regression-only
/// tripwire.
pub struct ResponseExtensionsAdvertisedRule;

impl CrossMessageAuditRule for ResponseExtensionsAdvertisedRule {
    fn name(&self) -> &'static str {
        "rfc3261.responseExtensionsAdvertised"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut invite_require_by_branch: HashMap<String, Vec<String>> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Request(req))
                            if req.method.as_str().eq_ignore_ascii_case("INVITE") =>
                        {
                            let branch = branch_of(msg);
                            if branch.is_empty() || invite_require_by_branch.contains_key(&branch) {
                                continue;
                            }
                            let require = collect_option_tags(&all_header_values(msg, "require"));
                            invite_require_by_branch.insert(branch, require);
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            if !cseq_method(msg).eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            let st = status(msg);
                            if st == 420 || st == 421 {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let Some(invite_require) = invite_require_by_branch.get(&branch) else {
                                continue;
                            };
                            if invite_require.is_empty() {
                                continue;
                            }
                            let mut advertised: HashSet<String> =
                                collect_option_tags(&all_header_values(msg, "supported"))
                                    .into_iter()
                                    .collect();
                            advertised.extend(collect_option_tags(&all_header_values(
                                msg, "require",
                            )));
                            let missing: Vec<&String> = invite_require
                                .iter()
                                .filter(|t| !advertised.contains(t.as_str()))
                                .collect();
                            if missing.is_empty() {
                                continue;
                            }
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent INVITE response (status {st}, branch {branch}) accepted \
                                     INVITE with Require=[{}] but did not advertise {} in \
                                     Supported/Require — {{Uas}} RFC 3261 §8.2.2.1 / \
                                     RFC3261-MUST-037",
                                    invite_require.join(", "),
                                    missing
                                        .iter()
                                        .map(|s| s.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.registerNoRouteSet
// ---------------------------------------------------------------------------

/// **RFC 3261 §10.2 — a REGISTER MUST NOT carry a Route header.** A REGISTER does
/// not establish a dialog or form a route set, so a Route header on it is a
/// defect. A real registrar / outbound proxy would mis-route; the test UA does
/// not police it. Regression-only tripwire (the "no dialog formed" half is
/// builder-guaranteed; this covers the observable Route-absence half).
pub struct RegisterNoRouteSetRule;

impl CrossMessageAuditRule for RegisterNoRouteSetRule {
    fn name(&self) -> &'static str {
        "rfc3261.registerNoRouteSet"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                for (kind, msg) in slot_events(&slot.ordered) {
                    if kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Request(req) = msg else {
                        continue;
                    };
                    if !req.method.as_str().eq_ignore_ascii_case("REGISTER") {
                        continue;
                    }
                    if all_header_values(msg, "route").is_empty() {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent REGISTER (callId {}) carries Route header — {{Uac}} RFC 3261 \
                             §10.2 / RFC3261-MUST-051",
                            call_id(msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.optionsResponseEchoes  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3261 §11.2 — a 2xx OPTIONS response SHOULD carry Allow/Supported/Accept.**
/// A UAS answering OPTIONS describes its capabilities the way an INVITE response
/// would. Per-branch: for each received OPTIONS, find the sent 2xx on the same
/// branch and fire if none of Allow/Supported/Accept appears.
///
/// **Advisory** (mirrors the TS `severityOverride: "advisory"`): the B2BUA emits
/// OPTIONS-keepalive 200s (ADR-0008 two-tier OPTIONS) that intentionally omit
/// capability headers — they are transport health probes, not §11.2 capability
/// discovery. Advisory until the subject narrows to genuine capability OPTIONS
/// or the probe responses opt in.
pub struct OptionsResponseEchoesRule;

impl CrossMessageAuditRule for OptionsResponseEchoesRule {
    fn name(&self) -> &'static str {
        "rfc3261.optionsResponseEchoes"
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut options_by_branch: HashMap<String, String> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Request(req))
                            if req.method.as_str().eq_ignore_ascii_case("OPTIONS") =>
                        {
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            options_by_branch
                                .entry(branch)
                                .or_insert_with(|| call_id(msg).to_string());
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            if !cseq_method(msg).eq_ignore_ascii_case("OPTIONS") {
                                continue;
                            }
                            let st = status(msg);
                            if !(200..300).contains(&st) {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let Some(cid) = options_by_branch.get(&branch) else {
                                continue;
                            };
                            let has = !all_header_values(msg, "allow").is_empty()
                                || !all_header_values(msg, "supported").is_empty()
                                || !all_header_values(msg, "accept").is_empty();
                            if has {
                                continue;
                            }
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent 2xx OPTIONS response (callId {cid}, branch {branch}) \
                                     lacks Allow/Supported/Accept headers — {{Uas}} RFC 3261 \
                                     §11.2 / RFC3261-MUST-059"
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.concurrentReInvite500or491
// ---------------------------------------------------------------------------

/// **RFC 3261 §14.2 — a re-INVITE racing a pending one MUST get 491 or 500.**
/// A UAS receiving a re-INVITE while a prior INVITE transaction in the same
/// dialog is still in progress MUST respond 491 (Request Pending) or 500 (Server
/// Internal Error) with Retry-After. A real UAS serialises offer/answer; the test
/// UA answers both. Regression-only tripwire.
pub struct ConcurrentReInvite500or491Rule;

impl CrossMessageAuditRule for ConcurrentReInvite500or491Rule {
    fn name(&self) -> &'static str {
        "rfc3261.concurrentReInvite500or491"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut in_progress_by_dialog: HashMap<String, HashSet<String>> = HashMap::new();
                let mut branch_to_dialog: HashMap<String, String> = HashMap::new();
                let mut concurrent_by_branch: HashMap<String, String> = HashMap::new();
                let mut response_by_branch: HashMap<String, (u16, bool)> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Request(req)) => {
                            if !req.method.as_str().eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            let tt = to_tag(msg).unwrap_or("");
                            let ft = from_tag(msg).unwrap_or("");
                            // Re-INVITE = in-dialog INVITE (both tags present).
                            if tt.is_empty() || ft.is_empty() {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let cid = call_id(msg);
                            let d_key = dialog_key(cid, ft, tt);
                            let d_key_alt = dialog_key(cid, tt, ft);
                            let in_a = in_progress_by_dialog.get(&d_key);
                            let in_b = in_progress_by_dialog.get(&d_key_alt);
                            // INVITE retransmit reuses the top-Via branch — skip.
                            let is_retransmit = in_a.map(|s| s.contains(&branch)).unwrap_or(false)
                                || in_b.map(|s| s.contains(&branch)).unwrap_or(false);
                            let prior_other_in_progress = !is_retransmit
                                && (in_a.map(|s| s.iter().any(|b| b != &branch)).unwrap_or(false)
                                    || in_b
                                        .map(|s| s.iter().any(|b| b != &branch))
                                        .unwrap_or(false));
                            if prior_other_in_progress {
                                concurrent_by_branch.insert(branch.clone(), cid.to_string());
                            }
                            in_progress_by_dialog
                                .entry(d_key.clone())
                                .or_default()
                                .insert(branch.clone());
                            branch_to_dialog.insert(branch, d_key);
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            if !cseq_method(msg).eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            if concurrent_by_branch.contains_key(&branch)
                                && !response_by_branch.contains_key(&branch)
                            {
                                let has_retry_after =
                                    !all_header_values(msg, "retry-after").is_empty();
                                response_by_branch
                                    .insert(branch.clone(), (status(msg), has_retry_after));
                            }
                            let st = status(msg);
                            if (200..700).contains(&st) {
                                if let Some(d_key) = branch_to_dialog.get(&branch) {
                                    if let Some(s) = in_progress_by_dialog.get_mut(d_key) {
                                        s.remove(&branch);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                for (branch, cid) in &concurrent_by_branch {
                    let resp = response_by_branch.get(branch);
                    if let Some((st, retry)) = resp {
                        if *st == 491 {
                            continue;
                        }
                        if *st == 500 && *retry {
                            continue;
                        }
                    }
                    let st = resp.map(|r| r.0).unwrap_or(0);
                    let has_retry = resp.map(|r| r.1).unwrap_or(false);
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Received concurrent re-INVITE (callId {cid}, branch {branch}) while \
                             prior INVITE in progress — {{Uas}} must respond 491 or \
                             500+Retry-After; got {st}/RetryAfter={has_retry}"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.noByeOutsideOrEarlyDialog
// ---------------------------------------------------------------------------

/// **RFC 3261 §15 — a BYE MUST NOT be sent outside a dialog or on an early one.**
/// A UA MUST NOT send BYE outside a dialog, and the callee (UAS) MUST NOT send
/// BYE on an early dialog (it should CANCEL or send a 4xx/5xx/6xx). A real peer
/// 481s such a BYE; the test UA answers it. Regression-only tripwire.
pub struct NoByeOutsideOrEarlyDialogRule;

impl CrossMessageAuditRule for NoByeOutsideOrEarlyDialogRule {
    fn name(&self) -> &'static str {
        "rfc3261.noByeOutsideOrEarlyDialog"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            let slice_has_confirmed_dialog = slice.to_tag.is_some();
            let slice_from = slice.from_tag.as_str();
            let slice_to = slice.to_tag.as_deref().unwrap_or("");
            for slot in &slice.per_agent {
                let mut agent_is_uas = false;
                let mut agent_sent_2xx = false;
                for (kind, msg) in slot_events(&slot.ordered) {
                    if kind == EventKind::Sent {
                        if let SipMessage::Response(_) = msg {
                            if !cseq_method(msg).eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            let resp_to_tag = to_tag(msg).unwrap_or("");
                            if resp_to_tag.is_empty()
                                || slice.to_tag.as_deref() != Some(resp_to_tag)
                            {
                                continue;
                            }
                            agent_is_uas = true;
                            let st = status(msg);
                            if (200..300).contains(&st) {
                                agent_sent_2xx = true;
                            }
                            continue;
                        }
                    }
                    let SipMessage::Request(req) = msg else {
                        continue;
                    };
                    if kind != EventKind::Sent {
                        continue;
                    }
                    if !req.method.as_str().eq_ignore_ascii_case("BYE") {
                        continue;
                    }
                    let cid = call_id(msg);
                    let ft = from_tag(msg).unwrap_or("");
                    let tt = to_tag(msg).unwrap_or("");
                    if !slice_has_confirmed_dialog || ft.is_empty() || tt.is_empty() {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Sent BYE (callId {cid}) outside any dialog — RFC 3261 §15 / \
                                 RFC3261-MUST-089"
                            ),
                        ));
                        continue;
                    }
                    let matches_slice = (ft == slice_from && tt == slice_to)
                        || (ft == slice_to && tt == slice_from);
                    if !matches_slice {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Sent BYE (callId {cid}) outside any dialog — RFC 3261 §15 / \
                                 RFC3261-MUST-089"
                            ),
                        ));
                        continue;
                    }
                    if agent_is_uas && !agent_sent_2xx {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Callee sent BYE on early dialog (callId {cid}) — should use \
                                 CANCEL or 4xx/5xx/6xx (RFC 3261 §15)"
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.noTarget404  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3261 §16.3 — a proxy with no resolvable target MUST respond 404.**
/// When a proxy cannot resolve any target for the Request-URI it MUST respond
/// 404 (Not Found). Per-slot: a received request whose branch was never
/// re-sent (not forwarded) but answered with a non-404 4xx/5xx/6xx final fires.
///
/// **Advisory** (mirrors the TS): the B2BUA worker is classified `proxy` for
/// subject dispatch but terminates each leg as UAC/UAS and may legitimately
/// reply 403/481/491 without forwarding when the backend rejects the call —
/// these are not "no target" outcomes. Advisory until the subject narrows to a
/// dedicated proxy bind.
pub struct NoTarget404Rule;

impl CrossMessageAuditRule for NoTarget404Rule {
    fn name(&self) -> &'static str {
        "rfc3261.noTarget404"
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut sent_branches: HashSet<String> = HashSet::new();
                let mut received_by_branch: HashMap<String, (String, String)> = HashMap::new();
                let mut final_by_branch: HashMap<String, u16> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    let branch = branch_of(msg);
                    if branch.is_empty() {
                        continue;
                    }
                    match (kind, msg) {
                        (EventKind::Sent, SipMessage::Request(_)) => {
                            sent_branches.insert(branch);
                        }
                        (EventKind::Received, SipMessage::Request(req)) => {
                            received_by_branch.entry(branch).or_insert((
                                req.method.as_str().to_string(),
                                call_id(msg).to_string(),
                            ));
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            let st = status(msg);
                            if st >= 200 {
                                final_by_branch.entry(branch).or_insert(st);
                            }
                        }
                        _ => {}
                    }
                }

                for (branch, (method, cid)) in &received_by_branch {
                    if sent_branches.contains(branch) {
                        continue;
                    }
                    let Some(&st) = final_by_branch.get(branch) else {
                        continue;
                    };
                    if !(400..700).contains(&st) || st == 404 {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "{{Proxy}} received request {method} (callId {cid}, branch {branch}) \
                             and emitted final response {st} without forwarding — expected 404 \
                             (RFC 3261 §16.3 / RFC3261-MUST-105)"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.unsupportedExtension421
// ---------------------------------------------------------------------------

/// **RFC 3261 §21.4.15 — a 421 response MUST carry a Require header.** When a UAS
/// sends 421 (Extension Required) it MUST list the extensions it requires of the
/// UAC in a Require header. A real UAC reads the list and retries; the test UA
/// does not. Regression-only tripwire.
pub struct UnsupportedExtension421Rule;

impl CrossMessageAuditRule for UnsupportedExtension421Rule {
    fn name(&self) -> &'static str {
        "rfc3261.unsupportedExtension421"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                for (kind, msg) in slot_events(&slot.ordered) {
                    if kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Response(_) = msg else {
                        continue;
                    };
                    if status(msg) != 421 {
                        continue;
                    }
                    let require = collect_option_tags(&all_header_values(msg, "require"));
                    if !require.is_empty() {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent 421 response (callId {}, branch {}) lacks Require header listing \
                             required extensions — {{Uas}} RFC 3261 §21.4.15 / RFC3261-MUST-182",
                            call_id(msg),
                            branch_of(msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.ackRequireSubsetOfInvite
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.2.2.4 — an ACK's Require MUST be a subset of the INVITE's.**
/// An ACK to a 2xx INVITE MUST contain only the Require option tags present in
/// the original INVITE. A real peer rejects an ACK that escalates extensions;
/// the test UA does not. Branch-correlated (ACK reuses the INVITE branch).
/// Regression-only tripwire.
pub struct AckRequireSubsetOfInviteRule;

impl CrossMessageAuditRule for AckRequireSubsetOfInviteRule {
    fn name(&self) -> &'static str {
        "rfc3261.ackRequireSubsetOfInvite"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let idx = build_branch_index(events);
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Request(ack) = &ev.msg else {
                        continue;
                    };
                    if !ack.method.as_str().eq_ignore_ascii_case("ACK") {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let Some(invite) = idx.find_invite_by_branch(&branch, Direction::Sent) else {
                        continue;
                    };
                    let ack_tags = split_option_tags(header_values_owned(&ev.msg, "require"));
                    if ack_tags.is_empty() {
                        continue;
                    }
                    let invite_tags = split_option_tags(header_values(invite, "require"));
                    let invite_set: HashSet<&String> = invite_tags.iter().collect();
                    let extras: Vec<&String> =
                        ack_tags.iter().filter(|t| !invite_set.contains(t)).collect();
                    if extras.is_empty() {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent ACK Require=[{}] not a subset of INVITE Require=[{}] (callId {}, \
                             branch {branch}) — RFC 3261 §13.2.2.4 / RFC3261-MUST-035",
                            ack_tags.join(", "),
                            invite_tags.join(", "),
                            call_id(&ev.msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.cancelRouteEchoesInvite
// ---------------------------------------------------------------------------

/// **RFC 3261 §9.1 — a CANCEL MUST carry the same Route values as the INVITE.**
/// A CANCEL takes the same path as the INVITE it cancels, so its Route set must
/// match. CANCEL shares the INVITE's top-Via branch (§9.1), so a per-branch
/// index pairs them. A real downstream proxy mis-routes a divergent CANCEL; the
/// test UA does not. Regression-only tripwire.
pub struct CancelRouteEchoesInviteRule;

impl CrossMessageAuditRule for CancelRouteEchoesInviteRule {
    fn name(&self) -> &'static str {
        "rfc3261.cancelRouteEchoesInvite"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let idx = build_branch_index(events);
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Request(cancel) = &ev.msg else {
                        continue;
                    };
                    if !cancel.method.as_str().eq_ignore_ascii_case("CANCEL") {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let Some(invite) = idx.find_invite_by_branch(&branch, Direction::Sent) else {
                        continue;
                    };
                    let cancel_routes = all_header_values(&ev.msg, "route");
                    let invite_routes = header_values(invite, "route");
                    if routes_equal(&cancel_routes, &invite_routes) {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent CANCEL Route values [{}] differ from INVITE Route values [{}] \
                             (callId {}, branch {branch}) — RFC 3261 §9.1 / RFC3261-MUST-046",
                            cancel_routes.join(", "),
                            invite_routes.join(", "),
                            call_id(&ev.msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.cancelAfter1xx  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3261 §9.1 — a UAC MUST NOT CANCEL before receiving a 1xx.** A CANCEL may
/// only be sent for an INVITE once at least one provisional response has arrived.
/// CANCEL shares the INVITE branch; we track the earliest received response per
/// branch and fire when a sent CANCEL has no prior 1xx (no response or only a
/// final).
///
/// **Advisory** (mirrors the TS): several fixtures legitimately fire CANCEL on a
/// UAC-local timer before the first 1xx (transient failure injection, glare).
/// Advisory until a per-fixture annotation distinguishes "spec-required wait"
/// from "fixture-driven race".
pub struct CancelAfter1xxRule;

impl CrossMessageAuditRule for CancelAfter1xxRule {
    fn name(&self) -> &'static str {
        "rfc3261.cancelAfter1xx"
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        // Whole-recording branch index — the conservative backstop. The
        // projector splits a CANCEL (To-tag absent) into its own pending slice,
        // away from the INVITE+1xx that confirmed the dialog (To-tag present),
        // so a CANCEL's own slot may not contain the 1xx that legitimately
        // preceded it. Before firing we confirm — across the *whole* recording —
        // that no 1xx was received on the CANCEL's branch; if one was, the
        // CANCEL is clean and we must not false-fire (cannot-judge ⇒ silent).
        let idx = build_branch_index(events);
        let received_1xx_on_branch = |branch: &str| -> bool {
            idx.responses_for(branch, Direction::Received)
                .iter()
                .any(|r| (100..200).contains(&status(&r.msg)))
        };
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                // First received response status per branch, in walk order — a
                // live walk enforces the "before" ordering a whole-stream lookup
                // would lose.
                let mut first_received_status: HashMap<String, u16> = HashMap::new();
                for (kind, msg) in slot_events(&slot.ordered) {
                    let branch = branch_of(msg);
                    if branch.is_empty() {
                        continue;
                    }
                    match (kind, msg) {
                        (EventKind::Received, SipMessage::Response(_)) => {
                            first_received_status.entry(branch).or_insert(status(msg));
                        }
                        (EventKind::Sent, SipMessage::Request(req))
                            if req.method.as_str().eq_ignore_ascii_case("CANCEL") =>
                        {
                            if let Some(&earliest) = first_received_status.get(&branch) {
                                if earliest < 200 {
                                    continue;
                                }
                            }
                            // Backstop: a 1xx for this branch anywhere in the
                            // recording means the CANCEL was after a provisional;
                            // the in-slot view just lost it to a slice split.
                            if received_1xx_on_branch(&branch) {
                                continue;
                            }
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent CANCEL (callId {}, branch {branch}) before any received \
                                     1xx for the INVITE — {{Uac}} RFC 3261 §9.1 / RFC3261-MUST-048",
                                    call_id(msg),
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.serialRegister
// ---------------------------------------------------------------------------

/// **RFC 3261 §10.2 — REGISTERs with differing Contacts MUST be serialised.** A
/// UAC MUST NOT send a REGISTER with a different Contact than a prior REGISTER
/// for the same AOR until the prior one has a final response. A real registrar
/// races the bindings; the test UA does not police it. Regression-only tripwire.
pub struct SerialRegisterRule;

impl CrossMessageAuditRule for SerialRegisterRule {
    fn name(&self) -> &'static str {
        "rfc3261.serialRegister"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let idx = build_branch_index(events);
                // AOR (To-URI) → in-flight (branch, contact).
                let mut in_flight_by_aor: HashMap<String, (String, String)> = HashMap::new();

                let has_final_received = |branch: &str| -> bool {
                    idx.responses_for(branch, Direction::Received)
                        .iter()
                        .any(|r| status(&r.msg) >= 200)
                };

                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Request(reg) = &ev.msg else {
                        continue;
                    };
                    if !reg.method.as_str().eq_ignore_ascii_case("REGISTER") {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let aor = to_uri(&ev.msg).to_string();
                    let contact = all_header_values(&ev.msg, "contact").join(",");

                    if let Some((prior_branch, _)) = in_flight_by_aor.get(&aor) {
                        if has_final_received(prior_branch) {
                            in_flight_by_aor.remove(&aor);
                        }
                    }
                    match in_flight_by_aor.get(&aor) {
                        Some((pending_branch, pending_contact)) if pending_contact != &contact => {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent REGISTER (callId {}, branch {branch}) with new Contact \
                                     while prior REGISTER (branch {pending_branch}) for AOR {aor} \
                                     still pending — {{Uac}} RFC 3261 §10.2 / RFC3261-MUST-054",
                                    call_id(&ev.msg),
                                ),
                            ));
                        }
                        Some(_) => {}
                        None => {
                            in_flight_by_aor.insert(aor, (branch, contact));
                        }
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.noReInviteWhileInviteInProgress
// ---------------------------------------------------------------------------

/// **RFC 3261 §14.1 — a UAC MUST NOT issue a re-INVITE while one is in progress.**
/// A new in-dialog INVITE MUST NOT be sent while a prior INVITE transaction is
/// still outstanding (no final received). A real peer 491s the second; the test
/// UA answers both. Regression-only tripwire (also covers RFC3261-MUST-084).
pub struct NoReInviteWhileInviteInProgressRule;

impl CrossMessageAuditRule for NoReInviteWhileInviteInProgressRule {
    fn name(&self) -> &'static str {
        "rfc3261.noReInviteWhileInviteInProgress"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let mut in_progress_by_dialog: HashMap<String, HashSet<String>> = HashMap::new();
                let mut branch_to_dialog: HashMap<String, String> = HashMap::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    match (kind, msg) {
                        (EventKind::Sent, SipMessage::Request(req)) => {
                            if !req.method.as_str().eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let cid = call_id(msg);
                            let ft = from_tag(msg).unwrap_or("");
                            let tt = to_tag(msg).unwrap_or("");
                            if !tt.is_empty() && !ft.is_empty() {
                                let d_key = dialog_key(cid, ft, tt);
                                let d_key_alt = dialog_key(cid, tt, ft);
                                let in_a = in_progress_by_dialog.get(&d_key);
                                let in_b = in_progress_by_dialog.get(&d_key_alt);
                                let is_retransmit =
                                    in_a.map(|s| s.contains(&branch)).unwrap_or(false)
                                        || in_b.map(|s| s.contains(&branch)).unwrap_or(false);
                                let prior = in_a
                                    .and_then(|s| s.iter().find(|b| *b != &branch))
                                    .or_else(|| {
                                        in_b.and_then(|s| s.iter().find(|b| *b != &branch))
                                    });
                                if !is_retransmit {
                                    if let Some(prior_branch) = prior {
                                        out.push((
                                            slot.bind_key.clone(),
                                            format!(
                                                "Sent re-INVITE (callId {cid}, branch {branch}) \
                                                 while prior INVITE (branch {prior_branch}) still \
                                                 in progress — {{Uac}} RFC 3261 §14.1 / \
                                                 RFC3261-MUST-083"
                                            ),
                                        ));
                                    }
                                }
                                in_progress_by_dialog
                                    .entry(d_key.clone())
                                    .or_default()
                                    .insert(branch.clone());
                                branch_to_dialog.insert(branch, d_key);
                            } else {
                                // Initial INVITE: no dialog yet, key by From-tag only.
                                let from_only = dialog_key(cid, ft, "");
                                in_progress_by_dialog
                                    .entry(from_only.clone())
                                    .or_default()
                                    .insert(branch.clone());
                                branch_to_dialog.insert(branch, from_only);
                            }
                        }
                        (EventKind::Received, SipMessage::Response(_)) => {
                            if !cseq_method(msg).eq_ignore_ascii_case("INVITE") {
                                continue;
                            }
                            if status(msg) < 200 {
                                continue;
                            }
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let Some(d_key) = branch_to_dialog.get(&branch).cloned() else {
                                continue;
                            };
                            if let Some(s) = in_progress_by_dialog.get_mut(&d_key) {
                                s.remove(&branch);
                            }
                            // Migrate the From-only entry to the full dialog key
                            // once the dialog identifier is complete.
                            let cid = call_id(msg);
                            let ft = from_tag(msg).unwrap_or("");
                            let tt = to_tag(msg).unwrap_or("");
                            if !ft.is_empty() && !tt.is_empty() {
                                let full_key = dialog_key(cid, ft, tt);
                                if full_key != d_key {
                                    let alt = dialog_key(cid, tt, ft);
                                    in_progress_by_dialog.entry(full_key).or_default();
                                    in_progress_by_dialog.entry(alt).or_default();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.proxy100WithinT100ms  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3261 §16.7 — a stateful proxy MUST send 100 Trying within T1.** A proxy
/// relaying an INVITE must emit 100 within ~200ms of receipt. The recorded event
/// model does not expose a per-message `at_ms` to the rule the way the TS stream
/// does, so — exactly like the TS — this degrades to a structural "100 was sent
/// on the same branch" check and CANNOT enforce the timing bound. When it cannot
/// judge the timing it returns nothing (no false positives).
///
/// **Advisory** (mirrors the TS): the B2BUA TransactionLayer does emit 100 Trying
/// on inbound INVITE and absorbs inbound 100s; the rule still fires on some
/// fixtures (a bypass code path or a branch-lookup heuristic gap across the
/// projector's bucket migration). `at_ms` is also missing so the 200ms bound
/// cannot be enforced. Advisory until the code path is found or the heuristic
/// corrected.
pub struct Proxy100WithinT100msRule;

impl CrossMessageAuditRule for Proxy100WithinT100msRule {
    fn name(&self) -> &'static str {
        "rfc3261.proxy100WithinT100ms"
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let idx = build_branch_index(events);
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Received {
                        continue;
                    }
                    let SipMessage::Request(invite) = &ev.msg else {
                        continue;
                    };
                    if !invite.method.as_str().eq_ignore_ascii_case("INVITE") {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let sent_100 = idx
                        .responses_for(&branch, Direction::Sent)
                        .iter()
                        .any(|r| {
                            status(&r.msg) == 100
                                && cseq_method(&r.msg).eq_ignore_ascii_case("INVITE")
                        });
                    if sent_100 {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "{{Proxy}} did not emit 100 Trying within 200ms of INVITE receipt \
                             (callId {}, branch {branch}; observed Δ=<ms>ms or no 100) — RFC 3261 \
                             §16.7 / RFC3261-MUST-095",
                            call_id(&ev.msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.strictRouteRewriteHandled
// ---------------------------------------------------------------------------

/// **RFC 3261 §16.4 — a proxy MUST apply the strict-route rewrite.** A request
/// whose topmost Route URI lacks `;lr` (strict route) MUST have that URI swapped
/// into the outgoing Request-URI before forwarding. A real proxy performs the
/// §16.4 swap; the test UA forwards verbatim. Regression-only tripwire (current
/// fixtures use loose routing).
pub struct StrictRouteRewriteHandledRule;

impl CrossMessageAuditRule for StrictRouteRewriteHandledRule {
    fn name(&self) -> &'static str {
        "rfc3261.strictRouteRewriteHandled"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                let idx = build_branch_index(events);
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Received {
                        continue;
                    }
                    let SipMessage::Request(req) = &ev.msg else {
                        continue;
                    };
                    let routes = all_header_values(&ev.msg, "route");
                    let Some(first_route) = routes.first() else {
                        continue;
                    };
                    let first_uri = extract_route_uri(first_route);
                    if route_is_loose(first_route) {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let method = req.method.as_str();
                    let sent = idx.requests_for(&branch, Direction::Sent);
                    let sent_req = sent.iter().find(|r| {
                        r.as_request()
                            .map(|sr| sr.method.as_str().eq_ignore_ascii_case(method))
                            .unwrap_or(false)
                    });
                    if let Some(sent_req) = sent_req {
                        if sent_req.as_request().map(|sr| sr.uri.as_str()) == Some(first_uri.as_str())
                        {
                            continue;
                        }
                    }
                    let sent_req_uri = sent_req
                        .and_then(|m| m.as_request())
                        .map(|sr| sr.uri.as_str())
                        .unwrap_or("<none>");
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "{{Proxy}} received strict-route request (callId {}, branch {branch}; \
                             first Route={first_uri}) but outgoing Request-URI={sent_req_uri} — \
                             RFC 3261 §16.4 / RFC3261-MUST-100",
                            call_id(&ev.msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.ackPreservesInviteRoute
// ---------------------------------------------------------------------------

/// **RFC 3261 §17.1.1.3 — the ACK for a non-2xx INVITE MUST keep the INVITE's
/// Route.** The non-2xx ACK shares the INVITE branch (§17.1.1.3) and MUST carry
/// the same Route values. An ACK whose branch carries no sent INVITE is an
/// ACK-for-2xx (different branch, different MUST) and is skipped. A real
/// downstream proxy mis-routes a divergent ACK; the test UA does not.
/// Regression-only tripwire.
pub struct AckPreservesInviteRouteRule;

impl CrossMessageAuditRule for AckPreservesInviteRouteRule {
    fn name(&self) -> &'static str {
        "rfc3261.ackPreservesInviteRoute"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let idx = build_branch_index(events);
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let SipMessage::Request(ack) = &ev.msg else {
                        continue;
                    };
                    if !ack.method.as_str().eq_ignore_ascii_case("ACK") {
                        continue;
                    }
                    let branch = branch_of(&ev.msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let Some(invite) = idx.find_invite_by_branch(&branch, Direction::Sent) else {
                        continue;
                    };
                    let ack_routes = all_header_values(&ev.msg, "route");
                    let invite_routes = header_values(invite, "route");
                    if routes_equal(&ack_routes, &invite_routes) {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent ACK Route values [{}] differ from INVITE Route values [{}] \
                             (callId {}, branch {branch}) — RFC 3261 §17.1.1.3 / RFC3261-MUST-145",
                            ack_routes.join(", "),
                            invite_routes.join(", "),
                            call_id(&ev.msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Local helpers shared across rules
// ---------------------------------------------------------------------------

/// Dialog key (Call-ID + an ordered tag pair) — the TS `dialogKey`.
fn dialog_key(call_id: &str, a: &str, b: &str) -> String {
    format!("{call_id}\x00{a}\x00{b}")
}

/// Owned copies of all values of `name` on a parsed message — needed because
/// [`split_option_tags`] consumes the iterator while `ev.msg` is borrowed.
fn header_values_owned(m: &SipMessage, name: &str) -> Vec<String> {
    all_header_values(m, name).into_iter().map(String::from).collect()
}

/// In-order equality of two Route header value lists (the TS `same` loop).
fn routes_equal(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(UnknownDialog481Rule),
        Arc::new(UnsupportedMethod405AllowRule),
        Arc::new(UnsupportedExtension420Rule),
        Arc::new(Unsupported415AcceptsRule),
        Arc::new(ResponseExtensionsAdvertisedRule),
        Arc::new(RegisterNoRouteSetRule),
        Arc::new(OptionsResponseEchoesRule),
        Arc::new(ConcurrentReInvite500or491Rule),
        Arc::new(NoByeOutsideOrEarlyDialogRule),
        Arc::new(NoTarget404Rule),
        Arc::new(UnsupportedExtension421Rule),
        Arc::new(AckRequireSubsetOfInviteRule),
        Arc::new(CancelRouteEchoesInviteRule),
        Arc::new(CancelAfter1xxRule),
        Arc::new(SerialRegisterRule),
        Arc::new(NoReInviteWhileInviteInProgressRule),
        Arc::new(Proxy100WithinT100msRule),
        Arc::new(StrictRouteRewriteHandledRule),
        Arc::new(AckPreservesInviteRouteRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    // ---- byte builders (mirroring cross_generic.rs / cseq.rs) -------------

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
                packet: UdpPacket { raw, src: src.parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    /// A request with caller-controlled method/branch/cseq/tags and an optional
    /// extra header block (e.g. `Require: ...\r\n`).
    #[allow(clippy::too_many_arguments)]
    fn req(
        method: &str,
        branch: &str,
        cseq: u32,
        from_uri: &str,
        to_uri: &str,
        ftag: &str,
        ttag: Option<&str>,
        extra: &str,
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
             {extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[allow(clippy::too_many_arguments)]
    fn resp(
        status: u16,
        cseq: u32,
        method: &str,
        from_uri: &str,
        to_uri: &str,
        ftag: &str,
        ttag: &str,
        branch: &str,
        extra: &str,
    ) -> Vec<u8> {
        // Empty `ttag` ⇒ a tagless To (e.g. 100 Trying). Emit a bare To rather
        // than `;tag=` (empty value), which is not a valid wire token and the
        // (even lenient) parser rejects with "Empty To tag parameter".
        let to = if ttag.is_empty() {
            format!("<{to_uri}>")
        } else {
            format!("<{to_uri}>;tag={ttag}")
        };
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{from_uri}>;tag={ftag}\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    const A: &str = "sip:alice@127.0.0.1";
    const B: &str = "sip:bob@127.0.0.1";

    // ---- unknownDialog481 -------------------------------------------------

    #[test]
    fn unknown_dialog_481_clean_when_confirmed() {
        // A normal confirmed dialog: INVITE + 200 + in-dialog BYE all land in
        // the confirmed slice, so the rule never fires.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            recv("bob", req("BYE", "z9hG4bK-b", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        assert!(UnknownDialog481Rule.check(&evs).is_empty());
    }

    #[test]
    fn unknown_dialog_481_tripwire_silent_for_orphan_both_tags() {
        // A received both-tags BYE carries To-tag=zz, so the projector keys it
        // straight into a *confirmed* (to_tag=Some) slice — it never lands in a
        // to_tag=None slice. Per the TS source this rule is a tripwire that
        // "never fires under normal slicing": it only inspects None-to_tag
        // slices, where a both-tags request landing would mean the projector
        // failed to learn the dialog. So a lone orphan both-tags request is NOT
        // flagged (the projector already matched it by construction). This pins
        // the documented invariant; the tripwire body is exercised below by
        // hand-driving the rule's per-slice logic over a synthesised None slice.
        let evs = vec![recv(
            "bob",
            req("BYE", "z9hG4bK-orphan", 2, A, B, "at", Some("zz"), ""),
            "127.0.0.1:5070",
            0,
        )];
        assert!(
            UnknownDialog481Rule.check(&evs).is_empty(),
            "orphan both-tags request lands in a confirmed slice, so the \
             None-slice tripwire stays silent"
        );

        // Tripwire body: only reachable if the projector ever lands a both-tags
        // received request in a None-to_tag slice (an invariant violation). We
        // synthesise exactly that slice and assert the rule flags it 481.
        use crate::rfc_audit::dialog_model::{AgentSlot, DialogSlice, EventKind, OrderedEvent};
        use sip_message::SipParser;
        let parser = super::super::lenient_parser();
        let bye = parser
            .parse(&req("BYE", "z9hG4bK-orphan", 2, A, B, "at", Some("zz"), ""))
            .expect("BYE parses");
        let slice = DialogSlice {
            call_id: "cid-1@127.0.0.1".to_string(),
            from_tag: "at".to_string(),
            to_tag: None,
            per_agent: vec![AgentSlot {
                bind_key: "bob".to_string(),
                ordered: vec![OrderedEvent {
                    kind: EventKind::Received,
                    idx: 0,
                    msg: bye,
                    wire_peer: None,
                }],
            }],
        };
        let f = UnknownDialog481Rule.check_slice(&slice);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("481"), "{}", f[0].1);
    }

    // ---- unsupportedMethod405Allow ---------------------------------------

    #[test]
    fn unsupported_method_405_clean() {
        let evs = vec![
            recv("bob", req("FROBNICATE", "z9hG4bK-x", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent(
                "bob",
                resp(405, 1, "FROBNICATE", A, B, "at", "bt", "z9hG4bK-x", "Allow: INVITE, BYE\r\n"),
                "127.0.0.1:5070",
                1,
            ),
        ];
        assert!(UnsupportedMethod405AllowRule.check(&evs).is_empty());
    }

    #[test]
    fn unsupported_method_405_flags_200() {
        let evs = vec![
            recv("bob", req("FROBNICATE", "z9hG4bK-x", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent(
                "bob",
                resp(200, 1, "FROBNICATE", A, B, "at", "bt", "z9hG4bK-x", ""),
                "127.0.0.1:5070",
                1,
            ),
        ];
        let f = UnsupportedMethod405AllowRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("405"), "{}", f[0].1);
    }

    // ---- unsupportedExtension420 -----------------------------------------

    #[test]
    fn unsupported_extension_420_clean() {
        let evs = vec![
            recv(
                "bob",
                req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Require: frobnicate\r\n"),
                "127.0.0.1:5070",
                0,
            ),
            sent(
                "bob",
                resp(420, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", "Unsupported: frobnicate\r\n"),
                "127.0.0.1:5070",
                1,
            ),
        ];
        assert!(UnsupportedExtension420Rule.check(&evs).is_empty());
    }

    #[test]
    fn unsupported_extension_420_flags_200() {
        let evs = vec![
            recv(
                "bob",
                req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Require: frobnicate\r\n"),
                "127.0.0.1:5070",
                0,
            ),
            sent(
                "bob",
                resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""),
                "127.0.0.1:5070",
                1,
            ),
        ];
        let f = UnsupportedExtension420Rule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("420"), "{}", f[0].1);
    }

    // ---- unsupported415Accepts -------------------------------------------

    #[test]
    fn unsupported_415_clean_with_accept() {
        let evs = vec![sent(
            "bob",
            resp(415, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", "Accept: application/sdp\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        assert!(Unsupported415AcceptsRule.check(&evs).is_empty());
    }

    #[test]
    fn unsupported_415_flags_missing_accept() {
        let evs = vec![sent(
            "bob",
            resp(415, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""),
            "127.0.0.1:5070",
            0,
        )];
        let f = Unsupported415AcceptsRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("415"), "{}", f[0].1);
    }

    // ---- responseExtensionsAdvertised ------------------------------------

    #[test]
    fn response_extensions_clean_when_echoed() {
        let evs = vec![
            recv(
                "bob",
                req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Require: 100rel\r\n"),
                "127.0.0.1:5070",
                0,
            ),
            sent(
                "bob",
                resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", "Supported: 100rel\r\n"),
                "127.0.0.1:5070",
                1,
            ),
        ];
        assert!(ResponseExtensionsAdvertisedRule.check(&evs).is_empty());
    }

    #[test]
    fn response_extensions_flags_unadvertised() {
        let evs = vec![
            recv(
                "bob",
                req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Require: 100rel\r\n"),
                "127.0.0.1:5070",
                0,
            ),
            sent(
                "bob",
                resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""),
                "127.0.0.1:5070",
                1,
            ),
        ];
        let f = ResponseExtensionsAdvertisedRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("100rel"), "{}", f[0].1);
    }

    // ---- registerNoRouteSet ----------------------------------------------

    #[test]
    fn register_no_route_clean() {
        let evs = vec![sent(
            "alice",
            req("REGISTER", "z9hG4bK-r", 1, A, A, "at", None, ""),
            "127.0.0.1:5070",
            0,
        )];
        assert!(RegisterNoRouteSetRule.check(&evs).is_empty());
    }

    #[test]
    fn register_with_route_flagged() {
        let evs = vec![sent(
            "alice",
            req("REGISTER", "z9hG4bK-r", 1, A, A, "at", None, "Route: <sip:p@h;lr>\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        let f = RegisterNoRouteSetRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("Route"), "{}", f[0].1);
    }

    // ---- optionsResponseEchoes [advisory] --------------------------------

    #[test]
    fn options_echo_is_advisory() {
        assert!(OptionsResponseEchoesRule.force_advisory());
    }

    #[test]
    fn options_echo_clean_with_allow() {
        let evs = vec![
            recv("bob", req("OPTIONS", "z9hG4bK-o", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent(
                "bob",
                resp(200, 1, "OPTIONS", A, B, "at", "bt", "z9hG4bK-o", "Allow: INVITE\r\n"),
                "127.0.0.1:5070",
                1,
            ),
        ];
        assert!(OptionsResponseEchoesRule.check(&evs).is_empty());
    }

    #[test]
    fn options_echo_flags_bare_200() {
        let evs = vec![
            recv("bob", req("OPTIONS", "z9hG4bK-o", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent(
                "bob",
                resp(200, 1, "OPTIONS", A, B, "at", "bt", "z9hG4bK-o", ""),
                "127.0.0.1:5070",
                1,
            ),
        ];
        let f = OptionsResponseEchoesRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("Allow/Supported/Accept"), "{}", f[0].1);
    }

    // ---- concurrentReInvite500or491 --------------------------------------

    #[test]
    fn concurrent_reinvite_clean_491() {
        // Two in-dialog INVITEs racing; the second gets 491.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            recv("bob", req("INVITE", "z9hG4bK-2", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 1),
            sent("bob", resp(491, 3, "INVITE", A, B, "at", "bt", "z9hG4bK-2", ""), "127.0.0.1:5070", 2),
            sent("bob", resp(200, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-1", ""), "127.0.0.1:5070", 3),
        ];
        assert!(ConcurrentReInvite500or491Rule.check(&evs).is_empty());
    }

    #[test]
    fn concurrent_reinvite_flags_200() {
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            recv("bob", req("INVITE", "z9hG4bK-2", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 1),
            sent("bob", resp(200, 3, "INVITE", A, B, "at", "bt", "z9hG4bK-2", ""), "127.0.0.1:5070", 2),
        ];
        let f = ConcurrentReInvite500or491Rule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("491 or 500"), "{}", f[0].1);
    }

    // ---- noByeOutsideOrEarlyDialog ---------------------------------------

    #[test]
    fn bye_in_confirmed_dialog_clean() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("BYE", "z9hG4bK-b", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        assert!(NoByeOutsideOrEarlyDialogRule.check(&evs).is_empty());
    }

    #[test]
    fn bye_outside_dialog_flagged() {
        // BYE with no To-tag → outside any dialog (slice to_tag=None).
        let evs = vec![sent(
            "alice",
            req("BYE", "z9hG4bK-b", 2, A, B, "at", None, ""),
            "127.0.0.1:5070",
            0,
        )];
        let f = NoByeOutsideOrEarlyDialogRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("outside any dialog"), "{}", f[0].1);
    }

    // ---- noTarget404 [advisory] ------------------------------------------

    #[test]
    fn no_target_404_is_advisory() {
        assert!(NoTarget404Rule.force_advisory());
    }

    #[test]
    fn no_target_404_clean_when_404() {
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("proxy", resp(404, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
        ];
        assert!(NoTarget404Rule.check(&evs).is_empty());
    }

    #[test]
    fn no_target_404_flags_500_unforwarded() {
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("proxy", resp(500, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
        ];
        let f = NoTarget404Rule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("expected 404"), "{}", f[0].1);
    }

    // ---- unsupportedExtension421 -----------------------------------------

    #[test]
    fn extension_421_clean_with_require() {
        let evs = vec![sent(
            "bob",
            resp(421, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", "Require: 100rel\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        assert!(UnsupportedExtension421Rule.check(&evs).is_empty());
    }

    #[test]
    fn extension_421_flags_missing_require() {
        let evs = vec![sent(
            "bob",
            resp(421, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""),
            "127.0.0.1:5070",
            0,
        )];
        let f = UnsupportedExtension421Rule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("Require"), "{}", f[0].1);
    }

    // ---- ackRequireSubsetOfInvite ----------------------------------------

    #[test]
    fn ack_require_subset_clean() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Require: 100rel\r\n"), "127.0.0.1:5070", 0),
            recv("alice", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("ACK", "z9hG4bK-i", 1, A, B, "at", Some("bt"), "Require: 100rel\r\n"), "127.0.0.1:5070", 2),
        ];
        assert!(AckRequireSubsetOfInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn ack_require_superset_flagged() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("ACK", "z9hG4bK-i", 1, A, B, "at", Some("bt"), "Require: timer\r\n"), "127.0.0.1:5070", 2),
        ];
        let f = AckRequireSubsetOfInviteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("subset"), "{}", f[0].1);
    }

    // ---- cancelRouteEchoesInvite -----------------------------------------

    #[test]
    fn cancel_route_clean_when_equal() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 0),
            sent("alice", req("CANCEL", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 1),
        ];
        assert!(CancelRouteEchoesInviteRule.check(&evs).is_empty());
    }

    #[test]
    fn cancel_route_flags_divergence() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 0),
            sent("alice", req("CANCEL", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:other@h;lr>\r\n"), "127.0.0.1:5070", 1),
        ];
        let f = CancelRouteEchoesInviteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("differ"), "{}", f[0].1);
    }

    // ---- cancelAfter1xx [advisory] ---------------------------------------

    #[test]
    fn cancel_after_1xx_is_advisory() {
        assert!(CancelAfter1xxRule.force_advisory());
    }

    #[test]
    fn cancel_after_1xx_clean() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(180, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("CANCEL", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 2),
        ];
        assert!(CancelAfter1xxRule.check(&evs).is_empty());
    }

    #[test]
    fn cancel_before_1xx_flagged() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("alice", req("CANCEL", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 1),
        ];
        let f = CancelAfter1xxRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("before any received"), "{}", f[0].1);
    }

    // ---- serialRegister --------------------------------------------------

    #[test]
    fn serial_register_clean_after_final() {
        let evs = vec![
            sent("alice", req("REGISTER", "z9hG4bK-1", 1, A, A, "at", None, "Contact: <sip:a@1>\r\n"), "127.0.0.1:5070", 0),
            recv("alice", resp(200, 1, "REGISTER", A, A, "at", "rt", "z9hG4bK-1", ""), "127.0.0.1:5070", 1),
            sent("alice", req("REGISTER", "z9hG4bK-2", 2, A, A, "at", None, "Contact: <sip:a@2>\r\n"), "127.0.0.1:5070", 2),
        ];
        assert!(SerialRegisterRule.check(&evs).is_empty());
    }

    #[test]
    fn serial_register_flags_concurrent_diff_contact() {
        let evs = vec![
            sent("alice", req("REGISTER", "z9hG4bK-1", 1, A, A, "at", None, "Contact: <sip:a@1>\r\n"), "127.0.0.1:5070", 0),
            sent("alice", req("REGISTER", "z9hG4bK-2", 2, A, A, "at", None, "Contact: <sip:a@2>\r\n"), "127.0.0.1:5070", 1),
        ];
        let f = SerialRegisterRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("still pending"), "{}", f[0].1);
    }

    // ---- noReInviteWhileInviteInProgress ---------------------------------

    #[test]
    fn no_reinvite_in_progress_clean() {
        // re-INVITE only after the first INVITE's final is received.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-1", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-1", ""), "127.0.0.1:5070", 1),
            sent("alice", req("INVITE", "z9hG4bK-2", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        assert!(NoReInviteWhileInviteInProgressRule.check(&evs).is_empty());
    }

    #[test]
    fn no_reinvite_in_progress_flags_race() {
        // First in-dialog INVITE still pending (no final) when a second fires.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            sent("alice", req("INVITE", "z9hG4bK-2", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 1),
        ];
        let f = NoReInviteWhileInviteInProgressRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("still"), "{}", f[0].1);
    }

    // ---- proxy100WithinT100ms [advisory] ---------------------------------

    #[test]
    fn proxy_100_is_advisory() {
        assert!(Proxy100WithinT100msRule.force_advisory());
    }

    #[test]
    fn proxy_100_clean_when_present() {
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("proxy", resp(100, 1, "INVITE", A, B, "at", "", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
        ];
        assert!(Proxy100WithinT100msRule.check(&evs).is_empty());
    }

    #[test]
    fn proxy_100_flags_missing() {
        let evs = vec![recv(
            "proxy",
            req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""),
            "127.0.0.1:5070",
            0,
        )];
        let f = Proxy100WithinT100msRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("100 Trying"), "{}", f[0].1);
    }

    // ---- strictRouteRewriteHandled ---------------------------------------

    #[test]
    fn strict_route_clean_loose() {
        // Loose route (;lr) → rule skips entirely.
        let evs = vec![recv(
            "proxy",
            req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        assert!(StrictRouteRewriteHandledRule.check(&evs).is_empty());
    }

    #[test]
    fn strict_route_flags_no_rewrite() {
        // Strict route (no ;lr) received, no matching forwarded request → flagged.
        let evs = vec![recv(
            "proxy",
            req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:strict@h>\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        let f = StrictRouteRewriteHandledRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("strict-route"), "{}", f[0].1);
    }

    // ---- ackPreservesInviteRoute -----------------------------------------

    #[test]
    fn ack_route_clean_when_equal() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 0),
            recv("alice", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("ACK", "z9hG4bK-i", 1, A, B, "at", Some("bt"), "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 2),
        ];
        assert!(AckPreservesInviteRouteRule.check(&evs).is_empty());
    }

    #[test]
    fn ack_route_flags_divergence() {
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, "Route: <sip:p@h;lr>\r\n"), "127.0.0.1:5070", 0),
            recv("alice", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("alice", req("ACK", "z9hG4bK-i", 1, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        let f = AckPreservesInviteRouteRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("differ"), "{}", f[0].1);
    }

    // ---- registration sanity ---------------------------------------------

    #[test]
    fn all_nineteen_rules_registered() {
        assert_eq!(cross_rules().len(), 19);
    }
}
