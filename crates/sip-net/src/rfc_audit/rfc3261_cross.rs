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
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::report::to_sip_entries;

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::types::UaRole;
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
                let mut completed_branches: HashSet<String> = HashSet::new();
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
                            // A same-branch repeat after its final is a §17.2.1
                            // retransmission (the server txn re-emits the
                            // response), not a new transaction — exempt, and it
                            // must NOT re-enter the in-progress window.
                            if completed_branches.contains(&branch) {
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
                                    completed_branches.insert(branch.clone());
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
/// 404 (Not Found). Per-slot: a received request that was never forwarded but
/// answered with a non-404 4xx/5xx/6xx final fires.
///
/// "Forwarded" is correlated by **Call-ID + CSeq** (method and number), NOT by
/// top-Via branch: an RFC 3261 §16.6-conformant proxy mints a FRESH branch on
/// the forwarded leg, so branch equality systematically misses the forward and
/// flagged every relaying proxy (the e2e LB false-positive class). A 4xx/5xx
/// final that merely RELAYS the downstream's rejection (the slot received the
/// same final status on the same Call-ID+CSeq) is also clean — the proxy did
/// resolve a target; the target said no.
///
/// **Advisory** (mirrors the TS): the B2BUA worker may legitimately reply
/// 403/481/491 without forwarding when the backend rejects the call — these are
/// not "no target" outcomes. Subject = `{Proxy}` so the rule no longer runs
/// against UA binds at all once roles are declared.
pub struct NoTarget404Rule;

impl CrossMessageAuditRule for NoTarget404Rule {
    fn name(&self) -> &'static str {
        "rfc3261.noTarget404"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Proxy])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                // (callId, cseq-num, cseq-method) the slot re-sent = forwarded.
                let mut forwarded: HashSet<(String, String)> = HashSet::new();
                let mut received_by_branch: HashMap<String, (String, String, String)> =
                    HashMap::new();
                let mut final_by_branch: HashMap<String, u16> = HashMap::new();
                // Final statuses RECEIVED per (callId, cseq) — a relayed
                // rejection is not a "no target" 404 case.
                let mut received_finals: HashSet<(String, String, u16)> = HashSet::new();

                for (kind, msg) in slot_events(&slot.ordered) {
                    let txn = format!("{}\x00{}", call_id(msg), cseq_of(msg));
                    match (kind, msg) {
                        (EventKind::Sent, SipMessage::Request(req)) => {
                            forwarded
                                .insert((txn, req.method.as_str().to_uppercase()));
                        }
                        (EventKind::Received, SipMessage::Request(req)) => {
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            received_by_branch.entry(branch).or_insert((
                                req.method.as_str().to_string(),
                                call_id(msg).to_string(),
                                txn,
                            ));
                        }
                        (EventKind::Received, SipMessage::Response(_)) => {
                            let st = status(msg);
                            if st >= 200 {
                                received_finals.insert((
                                    call_id(msg).to_string(),
                                    cseq_of(msg),
                                    st,
                                ));
                            }
                        }
                        (EventKind::Sent, SipMessage::Response(_)) => {
                            let branch = branch_of(msg);
                            if branch.is_empty() {
                                continue;
                            }
                            let st = status(msg);
                            if st >= 200 {
                                final_by_branch.entry(branch).or_insert(st);
                            }
                        }
                    }
                }

                for (branch, (method, cid, txn)) in &received_by_branch {
                    if forwarded.contains(&(txn.clone(), method.to_uppercase())) {
                        continue;
                    }
                    let Some(&st) = final_by_branch.get(branch) else {
                        continue;
                    };
                    if !(400..700).contains(&st) || st == 404 {
                        continue;
                    }
                    // A relayed downstream rejection (same status received on
                    // the same transaction) is not a no-target outcome.
                    let (c, q) = txn.split_once('\x00').unwrap_or((txn, ""));
                    if received_finals.contains(&(c.to_string(), q.to_string(), st)) {
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

/// `CSeq` value of a message as one token (number + method), for transaction
/// correlation that survives a proxy's per-hop branch rewrite.
fn cseq_of(m: &SipMessage) -> String {
    all_header_values(m, "cseq").first().map(|s| s.trim().to_string()).unwrap_or_default()
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
///
/// In-progress tracking keys on the ORDERED tag pair — (dialog id, requester
/// orientation) — NOT the unordered §12 dialog id: the merged per-dialog slice
/// gathers both From/To orientations of a dialog, and a forwarding slot (the
/// LB proxy's single bind) relays BOTH parties' crossing re-INVITEs, one hop
/// per direction of travel. Glare — one INVITE in progress per direction — is
/// legal (§14.1/§14.2, the peers 491/answer per leg); only a *same-direction*
/// overlap is one UAC violating §14.1 (newkahneed-030).
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
                                // ORDERED key: same-direction overlap only —
                                // the reversed orientation is the OTHER
                                // party's request (relayed glare), not this
                                // requester's (see the rule doc).
                                let d_key = dialog_key(cid, ft, tt);
                                let in_flight = in_progress_by_dialog.get(&d_key);
                                let is_retransmit =
                                    in_flight.map(|s| s.contains(&branch)).unwrap_or(false);
                                let prior =
                                    in_flight.and_then(|s| s.iter().find(|b| *b != &branch));
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
/// does — but the raw [`Stamped`] events DO carry `at_ms`, so this rule walks
/// them directly: a received INVITE is clean when the bind sent a 100 on the
/// same branch, OR sent ANY final (≥200) on that branch within the 200ms
/// window (RFC 3261 §16.7 only mandates the 100 when the final cannot be
/// produced promptly — a proxy that relays a 486 in 2ms owes nobody a Trying).
/// Otherwise it fires with the actually-observed Δ to the first final.
///
/// Subject = `{Proxy}` — §16.7 binds proxies; a UAS answering an INVITE is
/// governed by §8.2.6, not this rule. Stays **advisory**: under a paused test
/// clock a fixture may legitimately `advance` far past 200ms of virtual time
/// before answering, which is not a real-world latency.
pub struct Proxy100WithinT100msRule;

/// §16.7's "within 200ms" bound.
const PROXY_100_WINDOW_MS: u64 = 200;

impl CrossMessageAuditRule for Proxy100WithinT100msRule {
    fn name(&self) -> &'static str {
        "rfc3261.proxy100WithinT100ms"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Proxy])
    }

    fn force_advisory(&self) -> bool {
        true
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let parser = super::lenient_parser();
        // Per (bind, branch): INVITE receipt time + identity, the sent-100
        // flag, and the earliest sent-final time.
        let mut invites: HashMap<(String, String), (u64, String)> = HashMap::new();
        let mut sent_100: HashSet<(String, String)> = HashSet::new();
        let mut first_final: HashMap<(String, String), u64> = HashMap::new();

        for s in events {
            let (bind, raw, received) = match &s.event {
                SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                    (bind_key, &packet.raw, true)
                }
                SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                    (bind_key, msg, false)
                }
                _ => continue,
            };
            let Ok(msg) = parser.parse(raw) else { continue };
            let branch = branch_of(&msg);
            if branch.is_empty() || !cseq_method(&msg).eq_ignore_ascii_case("INVITE") {
                continue;
            }
            let key = (bind.clone(), branch);
            match (&msg, received) {
                (SipMessage::Request(r), true)
                    if r.method.as_str().eq_ignore_ascii_case("INVITE") =>
                {
                    invites
                        .entry(key)
                        .or_insert((s.at_ms, call_id(&msg).to_string()));
                }
                (SipMessage::Response(_), false) => {
                    let st = status(&msg);
                    if st == 100 {
                        sent_100.insert(key);
                    } else if st >= 200 {
                        let e = first_final.entry(key).or_insert(s.at_ms);
                        *e = (*e).min(s.at_ms);
                    }
                }
                _ => {}
            }
        }

        let mut out: Vec<(LaneKey, String)> = invites
            .into_iter()
            .filter_map(|((bind, branch), (t0, cid))| {
                if sent_100.contains(&(bind.clone(), branch.clone())) {
                    return None;
                }
                let observed = match first_final.get(&(bind.clone(), branch.clone())) {
                    Some(&tf) if tf.saturating_sub(t0) <= PROXY_100_WINDOW_MS => return None,
                    Some(&tf) => format!("first final after Δ={}ms", tf.saturating_sub(t0)),
                    None => "no response sent".to_string(),
                };
                Some((
                    bind,
                    format!(
                        "{{Proxy}} did not emit 100 Trying within {PROXY_100_WINDOW_MS}ms of \
                         INVITE receipt (callId {cid}, branch {branch}; {observed}) — RFC 3261 \
                         §16.7 / RFC3261-MUST-095"
                    ),
                ))
            })
            .collect();
        // HashMap iteration order is unstable; findings are reported in a
        // deterministic order for the report/dedup layers.
        out.sort();
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
/// fixtures use loose routing). Subject = `{Proxy}` — §16.4 is proxy behaviour;
/// a UA receiving a strict-Route request forwards nothing.
pub struct StrictRouteRewriteHandledRule;

impl CrossMessageAuditRule for StrictRouteRewriteHandledRule {
    fn name(&self) -> &'static str {
        "rfc3261.strictRouteRewriteHandled"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Proxy])
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
// rfc3261.unackedInvite2xxByed
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.3.1.4 — a 2xx to an INVITE that is never ACKed MUST be cleared
/// with a BYE.** A UAS (here the B2BUA a-leg) that answers an INVITE 2xx and then
/// never receives the matching ACK MUST retransmit the 2xx and, on giving up
/// (64·T1), send a BYE to clear the just-created dialog. A 2xx that is neither
/// ACKed nor BYE'd is the silent answered-call leak this audit catches: the
/// dialog stays live on the UAS forever (the test UA, which answers whatever it
/// is handed, never reveals it).
///
/// Per-UA dialog walk: for each slot, a SENT INVITE 2xx whose To-tag identifies a
/// confirmed dialog this agent is the UAS of opens an obligation; a subsequent
/// RECEIVED ACK on that dialog, or any BYE (sent or received) on it, discharges
/// it. An obligation still open at end-of-trace fires. Relay slots are skipped
/// (a transparent proxy forwards both directions and is not the dialog's UAS).
pub struct UnackedInvite2xxByedRule;

impl CrossMessageAuditRule for UnackedInvite2xxByedRule {
    fn name(&self) -> &'static str {
        "rfc3261.unackedInvite2xxByed"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        // This rule deliberately walks each BIND's whole stream rather than the
        // per-`(Call-ID, From-tag, To-tag)` dialog slices: the give-up BYE the UAS
        // sends carries its OWN tag in From (To/From swapped vs the answered
        // INVITE), so the 2xx and the discharging BYE land in DIFFERENT dialog
        // slices. Correlating them needs the bind-wide view, keyed by
        // `(Call-ID, UAS-tag)` where the UAS-tag is this agent's own dialog tag
        // (the To-tag it minted on the 2xx). Forking is handled naturally — each
        // fork's 2xx mints a distinct To-tag, so each opens its own obligation.
        let parser = crate::rfc_audit::lenient_parser();
        // bind → (Call-ID, uas-tag) → discharged?
        let mut per_bind: HashMap<LaneKey, HashMap<String, bool>> = HashMap::new();
        // Tags this agent owns per Call-ID (the From-tag of an INVITE it sent as
        // UAC, or the To-tag it minted as UAS) — so a BYE/ACK can be attributed to
        // the right side regardless of direction.
        for s in events {
            let (bind, raw, kind) = match &s.event {
                SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                    (bind_key, msg.as_slice(), EventKind::Sent)
                }
                SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                    (bind_key, packet.raw.as_slice(), EventKind::Received)
                }
                _ => continue,
            };
            let Ok(msg) = parser.parse(raw) else { continue };
            let cid = call_id(&msg);
            match &msg {
                // SENT 2xx to an INVITE → this agent is the UAS of the dialog its
                // own To-tag identifies; open the obligation.
                SipMessage::Response(_)
                    if kind == EventKind::Sent
                        && cseq_method(&msg).eq_ignore_ascii_case("INVITE")
                        && (200..300).contains(&status(&msg)) =>
                {
                    let uas_tag = to_tag(&msg).unwrap_or("");
                    if uas_tag.is_empty() {
                        continue;
                    }
                    per_bind
                        .entry(bind.clone())
                        .or_default()
                        .entry(dialog_key(cid, uas_tag, ""))
                        .or_insert(false);
                }
                // ACK / BYE (either direction) → the dialog is confirmed or being
                // cleared; discharge it. The UAS-tag is whichever of the two tags
                // names this bind's side — we cannot know which from the message
                // alone, so probe BOTH against the open obligations (a stray match
                // is impossible: the key also pins the Call-ID).
                SipMessage::Request(req)
                    if req.method.as_str().eq_ignore_ascii_case("ACK")
                        || req.method.as_str().eq_ignore_ascii_case("BYE") =>
                {
                    let ft = from_tag(&msg).unwrap_or("");
                    let tt = to_tag(&msg).unwrap_or("");
                    if let Some(m) = per_bind.get_mut(bind) {
                        for tag in [ft, tt] {
                            if tag.is_empty() {
                                continue;
                            }
                            if let Some(v) = m.get_mut(&dialog_key(cid, tag, "")) {
                                *v = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for (bind, obligations) in per_bind {
            for (key, discharged) in obligations {
                if !discharged {
                    let mut parts = key.split('\x00');
                    let cid = parts.next().unwrap_or("");
                    let tt = parts.next().unwrap_or("");
                    out.push((
                        bind.clone(),
                        format!(
                            "Sent a 2xx to an INVITE (callId {cid}, To-tag {tt}) that was never \
                             ACKed and the dialog was never BYE'd — the answered call leaks; \
                             RFC 3261 §13.3.1.4 requires retransmitting the 2xx and, on no ACK, \
                             BYE-ing the dialog"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.unackedInviteNon2xxFinal
// ---------------------------------------------------------------------------

/// **RFC 3261 §17.1.1.3 — a non-2xx INVITE final MUST be ACKed.** The UAS-side
/// obligation (newkahneed-033 ask E): a bind that received an INVITE and
/// answered it with a 3xx–6xx final must see the matching ACK — same Call-ID,
/// same top-Via branch (the non-2xx ACK belongs to the INVITE transaction and
/// reuses its branch; through the LB the arriving ACK is the upstream's own,
/// relayed on the INVITE's remembered hop with the proxy's forward branch —
/// exactly the branch this UAS saw on the INVITE). A final that is never ACKed
/// means
/// the rejecting UAS retransmits it to Timer H and the reject path never
/// cleanly completes — the exact class the `sipflow` pcap triage caught at
/// load ("486 never ACKed at the UAS") that no per-step `expect` sees.
///
/// **Gating** (promoted from advisory, newkahneed-034 ask B): the harness UAC
/// now has the §17.1.1.3 client-transaction behaviour — `ClientInvite` /
/// `ClientReinvite` / `InDialogTxn` auto-ACK any non-2xx INVITE final they
/// surface — so an undischarged obligation on the functional surface is a
/// genuine defect again (an unread reject, a peer that never ACKs, or the
/// "486 emitted but never delivered" class the load triage caught). A test
/// that deliberately models a peer which never ACKs waives with
/// `allow_violation`.
pub struct UnackedInviteNon2xxFinalRule;

impl CrossMessageAuditRule for UnackedInviteNon2xxFinalRule {
    fn name(&self) -> &'static str {
        "rfc3261.unackedInviteNon2xxFinal"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let parser = crate::rfc_audit::lenient_parser();
        // bind → (Call-ID \x00 branch) → discharged?
        let mut per_bind: HashMap<LaneKey, HashMap<String, bool>> = HashMap::new();
        // Branches this bind is the UAS of: it RECEIVED the initial INVITE.
        let mut uas_branches: HashMap<LaneKey, HashSet<String>> = HashMap::new();
        let key_of = |cid: &str, branch: &str| format!("{cid}\x00{branch}");
        for s in events {
            let (bind, raw, kind) = match &s.event {
                SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                    (bind_key, msg.as_slice(), EventKind::Sent)
                }
                SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                    (bind_key, packet.raw.as_slice(), EventKind::Received)
                }
                _ => continue,
            };
            let Ok(msg) = parser.parse(raw) else { continue };
            let branch = branch_of(&msg);
            if branch.is_empty() {
                continue;
            }
            let cid = call_id(&msg);
            match &msg {
                SipMessage::Request(req)
                    if kind == EventKind::Received
                        && req.method.as_str().eq_ignore_ascii_case("INVITE") =>
                {
                    uas_branches.entry(bind.clone()).or_default().insert(key_of(cid, &branch));
                }
                // The ACK for a non-2xx final reuses the INVITE's branch
                // (§17.1.1.3) → discharge that transaction's obligation.
                SipMessage::Request(req)
                    if kind == EventKind::Received
                        && req.method.as_str().eq_ignore_ascii_case("ACK") =>
                {
                    if let Some(m) = per_bind.get_mut(bind) {
                        if let Some(v) = m.get_mut(&key_of(cid, &branch)) {
                            *v = true;
                        }
                    }
                }
                // A SENT 3xx–6xx INVITE final on a branch we are the UAS of
                // opens the obligation (a retransmitted final re-uses the key).
                SipMessage::Response(_)
                    if kind == EventKind::Sent
                        && cseq_method(&msg).eq_ignore_ascii_case("INVITE")
                        && (300..700).contains(&status(&msg)) =>
                {
                    let key = key_of(cid, &branch);
                    if uas_branches.get(bind).is_some_and(|b| b.contains(&key)) {
                        per_bind.entry(bind.clone()).or_default().entry(key).or_insert(false);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for (bind, obligations) in per_bind {
            for (key, discharged) in obligations {
                if !discharged {
                    let mut parts = key.split('\x00');
                    let cid = parts.next().unwrap_or("");
                    let branch = parts.next().unwrap_or("");
                    out.push((
                        bind.clone(),
                        format!(
                            "Sent a non-2xx final to an INVITE (callId {cid}, branch {branch}) \
                             that was never ACKed — the INVITE transaction never completes and \
                             the reject retransmits to Timer H; RFC 3261 §17.1.1.3 makes the \
                             ACK mandatory (hop-by-hop, so a proxy in the path owes this UAS \
                             its own synthesized ACK)"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3261.failedReinviteTearsDownDialog
// ---------------------------------------------------------------------------

/// **RFC 3261 §14.1 — a failed re-INVITE MUST leave the dialog in its prior
/// state.** A non-2xx final to an in-dialog INVITE (a re-INVITE) does NOT tear
/// the call down: the prior session/media continues and the failure is merely
/// reported to the originator. A B2BUA that drops a re-INVITE's
/// transaction-correlation state on a relayed *provisional* (1xx) lets the
/// subsequent non-2xx final fall through its `route-failure` path and silently
/// destroys the bridged call — the originator's re-INVITE transaction then sees
/// a provisional with **no final** (RFC 3261 §17.1.1.2 also requires the UAS
/// transaction to deliver a final): the prior session is gone.
///
/// Per-transaction (top-Via branch) walk: for each SENT in-dialog INVITE (a
/// re-INVITE — To-tag already present, so the dialog is confirmed) that received
/// at least one *provisional* response on its branch but NEVER a *final*
/// (status >= 200), the transaction was abandoned. The only RFC-conformant ways
/// to end an in-dialog INVITE transaction are a 2xx or a non-2xx final; a
/// provisional-then-silence on a confirmed dialog is the §14.1 teardown this
/// audit catches (the silent-destroy variant — the buggy path emits no BYE at
/// all, so a BYE-keyed rule cannot see it; the abandoned transaction can).
///
/// **"absent another cause":** the rule fires only when NO BYE was observed on
/// that dialog (same bind, same Call-ID). An independent teardown — max-duration
/// (GlobalDuration), keepalive timeout, an ACK-timeout watchdog, or a peer BYE —
/// abandons any in-flight re-INVITE *as a side effect* but is itself signalled by
/// a BYE on the dialog, which is a legitimate §15 teardown for a different cause.
/// A re-INVITE provisional-without-final on a dialog that is *never* BYE'd is the
/// §14.1 violation: the failed re-INVITE silently dropped the prior session.
pub struct FailedReinviteTearsDownDialogRule;

impl CrossMessageAuditRule for FailedReinviteTearsDownDialogRule {
    fn name(&self) -> &'static str {
        "rfc3261.failedReinviteTearsDownDialog"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let idx = build_branch_index(events);
        // Per-bind set of Call-IDs that carried a BYE in either direction — the
        // "another cause" escape: an independent teardown is signalled by a BYE.
        let parser = crate::rfc_audit::lenient_parser();
        let mut byed_call_ids: HashMap<LaneKey, HashSet<String>> = HashMap::new();
        for s in events {
            let (bind, raw) = match &s.event {
                SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => (bind_key, msg.as_slice()),
                SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => (bind_key, packet.raw.as_slice()),
                _ => continue,
            };
            let Ok(msg) = parser.parse(raw) else { continue };
            if let SipMessage::Request(req) = &msg {
                if req.method.as_str().eq_ignore_ascii_case("BYE") {
                    byed_call_ids
                        .entry(bind.clone())
                        .or_default()
                        .insert(call_id(&msg).to_string());
                }
            }
        }
        let mut out = Vec::new();
        for (branch, entry) in &idx.by_branch {
            for sent in &entry.sent_requests {
                let Some(req) = sent.as_request() else { continue };
                // Only in-dialog INVITEs (re-INVITEs): a confirmed-dialog request
                // carries a To-tag. An initial INVITE (no To-tag) that is
                // abandoned is a different obligation (`unackedInvite2xx*` / txn
                // timeout) and not a §14.1 prior-state violation.
                if !req.method.as_str().eq_ignore_ascii_case("INVITE") {
                    continue;
                }
                let in_dialog = req
                    .to
                    .tag
                    .as_deref()
                    .map(|t| !t.is_empty())
                    .unwrap_or(false);
                if !in_dialog {
                    continue;
                }
                let responses = idx.responses_for(branch, Direction::Received);
                let saw_provisional = responses
                    .iter()
                    .any(|m| (100..200).contains(&m.as_response().map(|r| r.status).unwrap_or(0)));
                let saw_final = idx.has_final_response_for(branch, Direction::Received);
                if !saw_provisional || saw_final {
                    continue;
                }
                // "absent another cause": skip when the dialog was BYE'd (an
                // independent teardown legitimately abandons the re-INVITE).
                let cid = call_id(&sent.msg);
                let dialog_byed = byed_call_ids
                    .get(&sent.bind_key)
                    .map(|s| s.contains(cid))
                    .unwrap_or(false);
                if dialog_byed {
                    continue;
                }
                out.push((
                    sent.bind_key.clone(),
                    format!(
                        "re-INVITE (in-dialog INVITE, callId {cid}, branch {branch}) received a \
                         provisional but never a final response, and the dialog was never BYE'd — \
                         the prior dialog state was silently torn down on a failed re-INVITE; \
                         RFC 3261 §14.1 requires a failed re-INVITE to leave the dialog in its \
                         prior state (and §17.1.1.2 a final response)"
                    ),
                ));
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

// ===========================================================================
// rfc3261.no1xxAfterFinal  {Uas}
// ===========================================================================

/// **RFC 3261 §13.3.1.1 / §17.2.1 — no new 1xx after the final.** Once a UAS has
/// sent a final (≥200) on an INVITE server transaction, that transaction is
/// complete: it emits no further provisionals. A UAS that sends a NEW provisional
/// afterwards presents the caller an early dialog past the point the offer was
/// resolved. This is the general (any-1xx) sibling of
/// [`super::rfc3262_cross::NoNewReliable1xxAfterFinalRule`] (reliable-only,
/// keyed by RSeq); here a 1xx is identified by its (status, To-tag) — a distinct
/// early dialog / provisional — so a retransmitted final (same status) and a
/// retransmitted provisional (a 1xx whose FIRST emission was pre-final, even if
/// a copy is recorded after by reordering) never false-positive. Partitioned per
/// INVITE server transaction by (sender bind, top-Via branch); relay lanes are
/// skipped (a B2BUA face forwards its upstream's 1xx, it does not originate it).
pub struct No1xxAfterFinalRule;

impl CrossMessageAuditRule for No1xxAfterFinalRule {
    fn name(&self) -> &'static str {
        "rfc3261.no1xxAfterFinal"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>)> {
        // Relay faces merely FORWARD provisionals/finals — a 1xx a B2BUA relays
        // after a final is its upstream's emission, judged on the upstream lane,
        // not here (mirrors the reliable-1xx sibling's relay skip).
        let relay_lanes: HashSet<LaneKey> = project_per_dialog(events)
            .iter()
            .flat_map(|slice| slice.per_agent.iter())
            .filter(|slot| slot_is_relay(slot))
            .map(|slot| slot.bind_key.clone())
            .collect();

        // Per INVITE server transaction (sender, top-Via branch): the wire index
        // of the first final, and the (status, To-tag) of every 1xx whose FIRST
        // emission was seen — so a retransmission is never mistaken for a new one.
        #[derive(Default)]
        struct TxnState {
            final_seen: bool,
            seen_1xx: HashSet<(u16, String)>,
        }
        let mut txns: HashMap<(LaneKey, String), TxnState> = HashMap::new();
        let parser = CustomParser::new();
        let mut out = Vec::new();

        for (i, entry) in to_sip_entries(events).into_iter().enumerate() {
            let Some(sender) = entry.from_lane.clone() else { continue };
            if relay_lanes.contains(&sender) {
                continue;
            }
            let Ok(msg) = parser.parse(&entry.raw) else { continue };
            if !matches!(msg, SipMessage::Response(_)) || !cseq_method(&msg).eq_ignore_ascii_case("INVITE") {
                continue;
            }
            let Some(branch) = top_via_branch(&msg).filter(|b| !b.is_empty()) else { continue };
            let st = txns.entry((sender.clone(), branch)).or_default();
            let code = status(&msg);
            if code >= 200 {
                st.final_seen = true; // a retransmitted final repeats this — harmless
                continue;
            }
            if code <= 100 {
                continue; // 100 Trying establishes no early dialog
            }
            let ident = (code, to_tag(&msg).unwrap_or_default().to_string());
            if st.seen_1xx.contains(&ident) {
                continue; // a retransmission of an already-emitted provisional
            }
            if st.final_seen {
                out.push((
                    sender.clone(),
                    format!(
                        "Sent a new {code} provisional on an INVITE server transaction after its \
                         final response — a completed transaction emits no further provisionals \
                         (RFC 3261 §13.3.1.1 / §17.2.1)"
                    ),
                    Some(i + 1),
                ));
            }
            st.seen_1xx.insert(ident);
        }
        out
    }
}

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(No1xxAfterFinalRule),
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
        Arc::new(UnackedInvite2xxByedRule),
        Arc::new(UnackedInviteNon2xxFinalRule),
        Arc::new(FailedReinviteTearsDownDialogRule),
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
                disposition: crate::types::RecvDisposition::Delivered,
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
                proxy_only: false,
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

    #[test]
    fn concurrent_reinvite_post_final_retransmit_not_reinserted() {
        // newkahneed-035 regression: a Timer-A retransmission of an
        // already-finalised re-INVITE crosses its 491 on the wire and is
        // recorded after it. A same-branch INVITE repeat after its final is a
        // §17.2.1 retransmission (the server txn re-emits the response), not a
        // new transaction — it must not re-enter the in-progress window, so
        // the follow-up re-INVITE's compliant 200 stays clean.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            sent("bob", resp(491, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-1", ""), "127.0.0.1:5070", 1),
            // Post-final retransmit of the 491'd re-INVITE.
            recv("bob", req("INVITE", "z9hG4bK-1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
            // The DRR491 retry — the only re-INVITE genuinely in progress.
            recv("bob", req("INVITE", "z9hG4bK-2", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 3),
            sent("bob", resp(200, 3, "INVITE", A, B, "at", "bt", "z9hG4bK-2", ""), "127.0.0.1:5070", 4),
        ];
        let f = ConcurrentReInvite500or491Rule.check(&evs);
        assert!(f.is_empty(), "post-final retransmit re-inserted as concurrent re-INVITE: {f:?}");
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

    #[test]
    fn bye_after_uas_initiated_reinvite_tag_reversal_clean() {
        // newkahneed-029 regression: a mid-dialog re-INVITE initiated by the
        // UAS side (the AS sending a hold re-INVITE toward the caller) reverses
        // From/To tags relative to the dialog-establishing INVITE. With
        // From-tag-oriented bucketing, the LB relay's copy of the caller's 500
        // and of the AS's terminating BYE landed in a bucket that lacked the
        // dialog-confirming 200 under that key orientation, so the projector
        // misread a long-confirmed dialog's BYE as an early-dialog BYE and
        // falsely failed this rule (the identical wire passes on the direct
        // topology). Unordered tag-pair keying maps both orientations onto the
        // one dialog slice — the audit must stay clean on every slot.
        //
        // Complete callflow, relayed by a transparent LB ("lb" both receives
        // and re-sends every message; alice / bob are the dialog endpoints):
        // INVITE/200/ACK, then bob's re-INVITE (tags reversed) rejected 500 +
        // ACK, then bob's BYE answered 200.
        let al = "127.0.0.1:5060"; // alice <-> lb wire peer
        let lb = "127.0.0.1:5090"; // lb address as seen by both UAs
        let bl = "127.0.0.1:5070"; // bob <-> lb wire peer
        let evs = vec![
            // Establishment: alice -> lb -> bob, 200 back, ACK forward.
            sent("alice", req("INVITE", "z9hG4bK-i-a", 1, A, B, "at", None, ""), lb, 0),
            recv("lb", req("INVITE", "z9hG4bK-i-a", 1, A, B, "at", None, ""), al, 1),
            sent("lb", req("INVITE", "z9hG4bK-i-b", 1, A, B, "at", None, ""), bl, 2),
            recv("bob", req("INVITE", "z9hG4bK-i-b", 1, A, B, "at", None, ""), lb, 3),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i-b", ""), lb, 4),
            recv("lb", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i-b", ""), bl, 5),
            sent("lb", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i-a", ""), al, 6),
            recv("alice", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i-a", ""), lb, 7),
            sent("alice", req("ACK", "z9hG4bK-k-a", 1, A, B, "at", Some("bt"), ""), lb, 8),
            recv("lb", req("ACK", "z9hG4bK-k-a", 1, A, B, "at", Some("bt"), ""), al, 9),
            sent("lb", req("ACK", "z9hG4bK-k-b", 1, A, B, "at", Some("bt"), ""), bl, 10),
            recv("bob", req("ACK", "z9hG4bK-k-b", 1, A, B, "at", Some("bt"), ""), lb, 11),
            // UAS-initiated hold re-INVITE toward the caller: From/To reversed
            // relative to the establishing INVITE. Caller rejects it (500).
            sent("bob", req("INVITE", "z9hG4bK-r-b", 1, B, A, "bt", Some("at"), ""), lb, 12),
            recv("lb", req("INVITE", "z9hG4bK-r-b", 1, B, A, "bt", Some("at"), ""), bl, 13),
            sent("lb", req("INVITE", "z9hG4bK-r-a", 1, B, A, "bt", Some("at"), ""), al, 14),
            recv("alice", req("INVITE", "z9hG4bK-r-a", 1, B, A, "bt", Some("at"), ""), lb, 15),
            sent("alice", resp(500, 1, "INVITE", B, A, "bt", "at", "z9hG4bK-r-a", ""), lb, 16),
            recv("lb", resp(500, 1, "INVITE", B, A, "bt", "at", "z9hG4bK-r-a", ""), al, 17),
            sent("lb", resp(500, 1, "INVITE", B, A, "bt", "at", "z9hG4bK-r-b", ""), bl, 18),
            recv("bob", resp(500, 1, "INVITE", B, A, "bt", "at", "z9hG4bK-r-b", ""), lb, 19),
            // Non-2xx ACK (hop-by-hop, reuses the INVITE branch per hop).
            sent("bob", req("ACK", "z9hG4bK-r-b", 1, B, A, "bt", Some("at"), ""), lb, 20),
            recv("lb", req("ACK", "z9hG4bK-r-b", 1, B, A, "bt", Some("at"), ""), bl, 21),
            sent("lb", req("ACK", "z9hG4bK-r-a", 1, B, A, "bt", Some("at"), ""), al, 22),
            recv("alice", req("ACK", "z9hG4bK-r-a", 1, B, A, "bt", Some("at"), ""), lb, 23),
            // Terminating BYE from the AS side rides the confirmed dialog.
            sent("bob", req("BYE", "z9hG4bK-b-b", 2, B, A, "bt", Some("at"), ""), lb, 24),
            recv("lb", req("BYE", "z9hG4bK-b-b", 2, B, A, "bt", Some("at"), ""), bl, 25),
            sent("lb", req("BYE", "z9hG4bK-b-a", 2, B, A, "bt", Some("at"), ""), al, 26),
            recv("alice", req("BYE", "z9hG4bK-b-a", 2, B, A, "bt", Some("at"), ""), lb, 27),
            sent("alice", resp(200, 2, "BYE", B, A, "bt", "at", "z9hG4bK-b-a", ""), lb, 28),
            recv("lb", resp(200, 2, "BYE", B, A, "bt", "at", "z9hG4bK-b-a", ""), al, 29),
            sent("lb", resp(200, 2, "BYE", B, A, "bt", "at", "z9hG4bK-b-b", ""), bl, 30),
            recv("bob", resp(200, 2, "BYE", B, A, "bt", "at", "z9hG4bK-b-b", ""), lb, 31),
        ];
        let f = NoByeOutsideOrEarlyDialogRule.check(&evs);
        assert!(f.is_empty(), "confirmed-dialog BYE misread as early-dialog BYE: {f:?}");
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

    #[test]
    fn no_target_404_subject_is_proxy_only() {
        assert_eq!(NoTarget404Rule.subject(), HashSet::from([UaRole::Proxy]));
    }

    #[test]
    fn no_target_404_clean_when_forwarded_on_fresh_branch() {
        // A §16.6 proxy forwards the INVITE under a NEW branch and relays the
        // downstream 486 — that is a resolved target, not a "no target" case.
        // Branch-based forward detection used to miss this (the e2e LB false
        // positive); Call-ID+CSeq correlation must keep it clean.
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-in", 1, A, B, "at", None, ""), "127.0.0.1:5090", 0),
            sent("proxy", req("INVITE", "z9hG4bK-out", 1, A, B, "at", None, ""), "127.0.0.1:5070", 1),
            recv("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-out", ""), "127.0.0.1:5070", 2),
            sent("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-in", ""), "127.0.0.1:5090", 3),
        ];
        assert!(NoTarget404Rule.check(&evs).is_empty(), "{:?}", NoTarget404Rule.check(&evs));
    }

    #[test]
    fn no_target_404_clean_when_relaying_downstream_rejection() {
        // Even when the forwarded leg is not visible in this slot (e.g. it rode
        // a different bind), a sent final that MATCHES a received final on the
        // same Call-ID+CSeq is a relayed rejection, not a no-target outcome.
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-in", 1, A, B, "at", None, ""), "127.0.0.1:5090", 0),
            recv("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-other", ""), "127.0.0.1:5070", 1),
            sent("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-in", ""), "127.0.0.1:5090", 2),
        ];
        assert!(NoTarget404Rule.check(&evs).is_empty(), "{:?}", NoTarget404Rule.check(&evs));
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

    #[test]
    fn no_reinvite_in_progress_clean_on_relayed_crossing_glare() {
        // A forwarding slot (the LB proxy's single bind) relays BOTH parties'
        // crossing re-INVITEs — one hop per direction of travel. In the merged
        // §12 slice these are two requesters with one INVITE in progress each
        // (legal §14.1/§14.2 glare), not one UAC overlapping (newkahneed-030).
        let evs = vec![
            sent("proxy", req("INVITE", "z9hG4bK-a1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            sent("proxy", req("INVITE", "z9hG4bK-b1", 2, B, A, "bt", Some("at"), ""), "127.0.0.1:5060", 1),
            recv("proxy", resp(491, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-a1", ""), "127.0.0.1:5070", 2),
            recv("proxy", resp(491, 2, "INVITE", B, A, "bt", "at", "z9hG4bK-b1", ""), "127.0.0.1:5060", 3),
        ];
        let f = NoReInviteWhileInviteInProgressRule.check(&evs);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn no_reinvite_in_progress_same_direction_overlap_flags_amid_reversed_traffic() {
        // Ordered keying must not blind the rule: a genuinely overlapping
        // same-direction pair still flags (exactly once) with the other
        // party's reversed re-INVITE interleaved in the same merged slice.
        let evs = vec![
            sent("proxy", req("INVITE", "z9hG4bK-b1", 2, B, A, "bt", Some("at"), ""), "127.0.0.1:5060", 0),
            sent("proxy", req("INVITE", "z9hG4bK-a1", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 1),
            sent("proxy", req("INVITE", "z9hG4bK-a2", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        let f = NoReInviteWhileInviteInProgressRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("z9hG4bK-a1"), "{}", f[0].1);
        assert!(f[0].1.contains("z9hG4bK-a2"), "{}", f[0].1);
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
        assert!(f[0].1.contains("no response sent"), "{}", f[0].1);
    }

    #[test]
    fn proxy_100_subject_is_proxy_only() {
        assert_eq!(Proxy100WithinT100msRule.subject(), HashSet::from([UaRole::Proxy]));
    }

    #[test]
    fn proxy_100_clean_when_final_within_window() {
        // No 100 Trying, but the final went out 5ms after the INVITE — §16.7
        // only obliges the 100 when the final cannot be produced within 200ms.
        // (`sent`/`recv` stamp `at_ms = seq`.)
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 5),
        ];
        assert!(
            Proxy100WithinT100msRule.check(&evs).is_empty(),
            "{:?}",
            Proxy100WithinT100msRule.check(&evs)
        );
    }

    #[test]
    fn proxy_100_flags_final_past_window_with_observed_delta() {
        let evs = vec![
            recv("proxy", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("proxy", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 350),
        ];
        let f = Proxy100WithinT100msRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("Δ=350ms"), "{}", f[0].1);
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

    // ---- unackedInvite2xxByed --------------------------------------------

    #[test]
    fn unacked_2xx_clean_when_acked() {
        // bob (UAS): INVITE in, 200 out, ACK in → obligation discharged.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            recv("bob", req("ACK", "z9hG4bK-a", 1, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        assert!(UnackedInvite2xxByedRule.check(&evs).is_empty());
    }

    #[test]
    fn unacked_2xx_clean_when_byed() {
        // No ACK ever arrives, but the UAS BYEs the just-created dialog (the
        // RFC-correct give-up) → obligation discharged.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            // A retransmit of the same 2xx must not re-open the obligation.
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 2),
            // bob sends BYE: From = bob's own tag (bt), To = alice (at).
            sent("bob", req("BYE", "z9hG4bK-b", 2, B, A, "bt", Some("at"), ""), "127.0.0.1:5070", 3),
        ];
        assert!(UnackedInvite2xxByedRule.check(&evs).is_empty());
    }

    #[test]
    fn unacked_2xx_never_acked_nor_byed_is_flagged() {
        // The leak: bob answers 200 and the call is neither ACKed nor BYE'd.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
        ];
        let f = UnackedInvite2xxByedRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("never ACKed"), "{}", f[0].1);
        // A gating (non-advisory) finding.
        assert!(!UnackedInvite2xxByedRule.force_advisory());
    }

    // ---- unackedInviteNon2xxFinal ----------------------------------------

    #[test]
    fn unacked_non2xx_clean_when_acked_on_the_invite_branch() {
        // bob (UAS): INVITE in, 486 out, ACK in on the SAME branch (§17.1.1.3:
        // the non-2xx ACK belongs to the INVITE transaction) → discharged. A
        // retransmitted 486 must not re-open the obligation.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
            sent("bob", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 2),
            recv("bob", req("ACK", "z9hG4bK-i", 1, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 3),
        ];
        assert!(UnackedInviteNon2xxFinalRule.check(&evs).is_empty());
    }

    #[test]
    fn unacked_non2xx_never_acked_is_flagged_and_gates() {
        // The 033 wire finding: the 486 goes out, the mandatory ACK never
        // reaches this UAS (mis-demuxed / dropped at a hop) → one finding.
        let evs = vec![
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            sent("bob", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 1),
        ];
        let f = UnackedInviteNon2xxFinalRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("never ACKed"), "{}", f[0].1);
        // Gating since the harness UAC auto-ACKs non-2xx finals (034 ask B).
        assert!(!UnackedInviteNon2xxFinalRule.force_advisory());
    }

    #[test]
    fn unacked_non2xx_ignores_uac_side_finals_and_2xx() {
        // alice's bind RECEIVES a 486 (she is the UAC — the obligation is the
        // sender's), and bob's 2xx final is the OTHER rule's business: neither
        // opens an obligation here.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-u", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(486, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-u", ""), "127.0.0.1:5070", 1),
            recv("bob", req("INVITE", "z9hG4bK-i", 1, A, B, "at", None, ""), "127.0.0.1:5070", 2),
            sent("bob", resp(200, 1, "INVITE", A, B, "at", "bt", "z9hG4bK-i", ""), "127.0.0.1:5070", 3),
        ];
        assert!(UnackedInviteNon2xxFinalRule.check(&evs).is_empty());
    }

    // ---- failedReinviteTearsDownDialog -----------------------------------

    #[test]
    fn failed_reinvite_clean_when_final_relayed() {
        // alice's bind: a re-INVITE (in-dialog INVITE — To-tag present) that
        // receives 183 then 488 on its branch is clean (the final arrived).
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-re", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            recv("alice", resp(183, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-re", ""), "127.0.0.1:5070", 1),
            recv("alice", resp(488, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-re", ""), "127.0.0.1:5070", 2),
        ];
        assert!(FailedReinviteTearsDownDialogRule.check(&evs).is_empty());
    }

    #[test]
    fn failed_reinvite_provisional_then_silence_is_flagged() {
        // The bug: a re-INVITE gets a relayed 183 but the 488 final is never
        // delivered — the prior dialog state was silently torn down.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-re", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            recv("alice", resp(183, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-re", ""), "127.0.0.1:5070", 1),
        ];
        let f = FailedReinviteTearsDownDialogRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("silently torn down"), "{}", f[0].1);
        // A gating (non-advisory) finding.
        assert!(!FailedReinviteTearsDownDialogRule.force_advisory());
    }

    #[test]
    fn failed_reinvite_clean_when_dialog_byed_for_another_cause() {
        // A re-INVITE got a provisional and no final, BUT the dialog was BYE'd
        // (an independent teardown — max-duration, keepalive, peer BYE — that
        // legitimately abandons the in-flight re-INVITE). "absent another cause"
        // ⇒ NOT flagged.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-re", 2, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 0),
            recv("alice", resp(100, 2, "INVITE", A, B, "at", "bt", "z9hG4bK-re", ""), "127.0.0.1:5070", 1),
            // The dialog is torn down by a BYE (another cause).
            sent("alice", req("BYE", "z9hG4bK-bye", 3, A, B, "at", Some("bt"), ""), "127.0.0.1:5070", 2),
        ];
        assert!(FailedReinviteTearsDownDialogRule.check(&evs).is_empty());
    }

    #[test]
    fn failed_reinvite_initial_invite_not_flagged() {
        // An INITIAL INVITE (no To-tag) that gets a 100 Trying and is then
        // abandoned is NOT a §14.1 prior-state violation — a different
        // obligation owns it. This rule scopes strictly to in-dialog INVITEs.
        let evs = vec![
            sent("alice", req("INVITE", "z9hG4bK-init", 1, A, B, "at", None, ""), "127.0.0.1:5070", 0),
            recv("alice", resp(100, 1, "INVITE", A, B, "at", "", "z9hG4bK-init", ""), "127.0.0.1:5070", 1),
        ];
        assert!(FailedReinviteTearsDownDialogRule.check(&evs).is_empty());
    }

    // ---- no1xxAfterFinal --------------------------------------------------

    // `to_sip_entries` (the wire view the rule + waivers index) resolves lanes by
    // ADDRESS, so these traces use addr bind keys — bob = the UAS at :5070
    // sending to alice at :5060 (the harness uses the same addr lanes).
    const BOB: &str = "127.0.0.1:5070";
    const ALICE: &str = "127.0.0.1:5060";

    #[test]
    fn no_1xx_after_final_flags_new_provisional() {
        // bob: 180 (e1) → 200 (winner) → a NEW 181 after the final. The 181 is a
        // new provisional past transaction completion → flagged, pointing at it.
        let evs = vec![
            sent(BOB, resp(180, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 1),
            sent(BOB, resp(181, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 2),
        ];
        let out = No1xxAfterFinalRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "the new 1xx after the final is flagged");
        assert_eq!(out[0].0, BOB, "attributed to the UAS lane");
        assert_eq!(out[0].2, Some(3), "offending points at the 3rd wire entry (the late 181)");
    }

    #[test]
    fn no_1xx_after_final_silent_on_retransmitted_final_and_provisional() {
        // A retransmitted final (same status) and a retransmitted provisional (a
        // 1xx first emitted PRE-final, its copy recorded after) must NOT fire.
        let evs = vec![
            sent(BOB, resp(180, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 0),
            sent(BOB, resp(200, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 1),
            sent(BOB, resp(200, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 2),
            sent(BOB, resp(180, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 3),
        ];
        assert!(
            No1xxAfterFinalRule.check_positioned(&evs).is_empty(),
            "retransmitted final + reordered pre-final 1xx are not new emissions"
        );
    }

    #[test]
    fn no_1xx_after_final_silent_on_multi_early_dialog_before_final() {
        // The multi-early-dialog happy path: two distinct early dialogs (180 on
        // bt1, 183 on bt2) both BEFORE the 200 winner — no false positive.
        let evs = vec![
            sent(BOB, resp(180, 1, "INVITE", A, B, "at", "bt1", "z9hG4bK-i", ""), ALICE, 0),
            sent(BOB, resp(183, 1, "INVITE", A, B, "at", "bt2", "z9hG4bK-i", ""), ALICE, 1),
            sent(BOB, resp(200, 1, "INVITE", A, B, "at", "bt2", "z9hG4bK-i", ""), ALICE, 2),
        ];
        assert!(No1xxAfterFinalRule.check_positioned(&evs).is_empty());
    }

    // ---- registration sanity ---------------------------------------------

    #[test]
    fn all_cross_rules_registered() {
        assert_eq!(cross_rules().len(), 23);
    }
}
