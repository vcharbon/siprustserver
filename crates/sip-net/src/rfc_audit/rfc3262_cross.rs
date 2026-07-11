//! Port of `tests/harness/rules/rfc/rfc3262-cross-message-rules.ts` — the
//! RFC 3262 (reliable provisional response / PRACK / RAck / RSeq) cross-message
//! rules. These span more than one message — a UAS's reliable `1xx` and the
//! UAC's `PRACK` that acknowledges it, or an `INVITE`'s `Require:100rel` and the
//! disjunction of "reliable `1xx`" / "`420` Unsupported" it obliges — so they
//! live in the [`CrossMessageAuditRule`] family rather than the per-message peer
//! pack.
//!
//! Authoring pattern (mirrors [`super::cross_generic`]): a unit struct per rule
//! that projects the channel with [`project_per_dialog`], skips relay slots
//! (`slot_is_relay`) for the per-UA dialog walks, and reads
//! RSeq/RAck/Require/Supported/Unsupported via [`get_headers`] +
//! [`split_option_tags`] / [`parse_rack`]. PRACK↔reliable-1xx correlation keys
//! on the RAck `response-num` (= the 1xx's RSeq), per RFC 3262 §7.2.
//!
//! Subjects: the TS `adaptCrossMessageRule` maps every rule to `ALL_UA_ROLES`,
//! so each rule here keeps the default [`all_ua_roles`](crate::types::all_ua_roles)
//! subject; the conceptual `{Uas}` / `{Uac}` actor the rule judges is noted in
//! each doc comment. Three rules are TS-`severityOverride:"advisory"` and
//! override [`force_advisory`](CrossMessageAuditRule::force_advisory).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::get_headers;
use sip_message::parser::custom::structured_headers::{parse_rack, ParsedRack};
use sip_message::SipMessage;

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, msg_headers, project_per_dialog, slot_is_relay, status, top_via_branch,
    EventKind, OrderedEvent,
};
use crate::types::UaRole;
use crate::rfc_audit::txn_correlation::split_option_tags;

// ---------------------------------------------------------------------------
// Shared readers (ports of the TS `hasOptionTag`, RAck/RSeq scalar parsing).
// ---------------------------------------------------------------------------

/// True iff `header` (Require / Supported / Unsupported …) on `msg` lists the
/// `tag` option-tag (case-insensitive, comma-folded). Mirrors the TS
/// `hasOptionTag`.
fn has_option_tag(msg: &SipMessage, header: &str, tag: &str) -> bool {
    split_option_tags(get_headers(msg_headers(msg), header))
        .iter()
        .any(|t| t == tag)
}

/// All numeric RSeq values on `msg` (a reliable 1xx carries exactly one, but the
/// reader tolerates the lenient multi-header shape). Non-numeric values drop.
fn rseq_values(msg: &SipMessage) -> Vec<u64> {
    get_headers(msg_headers(msg), "rseq")
        .into_iter()
        .filter_map(|raw| raw.trim().parse::<u64>().ok())
        .collect()
}

/// The parsed RAck `(response-num, CSeq-num, method)` of a PRACK, or `None` when
/// the header is absent / unparseable (cannot-judge ⇒ SKIP). The first RAck
/// header wins, mirroring the TS `msg.getHeader("rack")`.
fn rack_of(msg: &SipMessage) -> Option<ParsedRack> {
    get_headers(msg_headers(msg), "rack")
        .into_iter()
        .find_map(parse_rack)
}

/// A reliable 1xx INVITE response: status in `101..=199`, CSeq method INVITE,
/// carrying `Require: 100rel`.
fn is_reliable_1xx(msg: &SipMessage) -> bool {
    matches!(msg, SipMessage::Response(_))
        && cseq_method(msg).eq_ignore_ascii_case("INVITE")
        && status(msg) > 100
        && status(msg) < 200
        && has_option_tag(msg, "require", "100rel")
}

/// Is this ordered event a `PRACK` request (sent or received)?
fn is_prack(ev: &OrderedEvent) -> bool {
    matches!(&ev.msg, SipMessage::Request(r) if r.method.as_str().eq_ignore_ascii_case("PRACK"))
}

/// The top-Via branch, or empty when absent — matches the TS `?? ""` guard so a
/// branch-less message is simply skipped by the `if branch.is_empty()` checks.
fn branch_of(msg: &SipMessage) -> String {
    top_via_branch(msg).unwrap_or_default()
}

// ===========================================================================
// rfc3262.requireReliable1xxOnRequire  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-001/-002) — honour `Require:100rel` on an INVITE.** A UAS
/// receiving an INVITE whose `Require` lists `100rel` MUST either send *every*
/// non-100 1xx reliably (`Require:100rel` + `RSeq`) OR reject the INVITE with a
/// `420 Bad Extension` carrying `Unsupported:100rel`. A real UAS enforces this
/// disjunction; the test UAS answers whatever it is scripted to, so a fixture
/// that emits a plain 18x against a `Require:100rel` INVITE (and no conforming
/// 420) slips through unless the recording is audited. Per-Via-branch dedup
/// folds INVITE retransmits.
pub struct RequireReliable1xxOnRequireRule;

impl CrossMessageAuditRule for RequireReliable1xxOnRequireRule {
    fn name(&self) -> &'static str {
        "rfc3262.requireReliable1xxOnRequire"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // branch → received INVITE's Call-ID (only those with Require:100rel).
                let mut requiring: HashMap<String, String> = HashMap::new();
                let mut satisfied_420: HashSet<String> = HashSet::new();
                // branch → list of offending 1xx statuses.
                let mut violations: HashMap<String, Vec<u16>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind == EventKind::Received {
                        if let SipMessage::Request(r) = msg {
                            if r.method.as_str().eq_ignore_ascii_case("INVITE")
                                && has_option_tag(msg, "require", "100rel")
                            {
                                let branch = branch_of(msg);
                                if !branch.is_empty() {
                                    requiring.entry(branch).or_insert_with(|| call_id(msg).to_string());
                                }
                            }
                        }
                        continue;
                    }
                    // Sent response on the INVITE transaction.
                    if !matches!(msg, SipMessage::Response(_))
                        || !cseq_method(msg).eq_ignore_ascii_case("INVITE")
                    {
                        continue;
                    }
                    let branch = branch_of(msg);
                    if branch.is_empty() || !requiring.contains_key(&branch) {
                        continue;
                    }
                    let st = status(msg);
                    if st == 420 && has_option_tag(msg, "unsupported", "100rel") {
                        satisfied_420.insert(branch);
                        continue;
                    }
                    if st <= 100 || st >= 200 {
                        continue;
                    }
                    let reliable = has_option_tag(msg, "require", "100rel")
                        && !rseq_values(msg).is_empty();
                    if reliable {
                        continue;
                    }
                    violations.entry(branch).or_default().push(st);
                }

                for (branch, cid) in &requiring {
                    if satisfied_420.contains(branch) {
                        continue;
                    }
                    let Some(list) = violations.get(branch) else {
                        continue;
                    };
                    for st in list {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "INVITE required 100rel (callId {cid}, branch {branch}) but sent \
                                 1xx response {st} lacks Require:100rel/RSeq and no 420 \
                                 Unsupported:100rel was sent — RFC 3262 §3 / RFC3262-MUST-001/-002"
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.reliableNeedsClientOptIn  {Uas}  [ADVISORY]
// ===========================================================================

/// **RFC 3262 §3 (MUST-004) — no reliable 1xx without client opt-in.** A UAS MUST
/// NOT send a reliable 1xx unless the matching INVITE carried `Supported:100rel`
/// or `Require:100rel`. Without opt-in the UAS has no licence to engage the PRACK
/// machinery.
///
/// **Advisory** (TS `severityOverride:"advisory"`): a B2BUA worker may emit a
/// reliable 18x on one leg when policy requires PRACK termination at the B2BUA.
/// The upstream INVITE on the other leg did opt in (`Supported:100rel`), but the
/// downstream INVITE the rule sees may not — the B2BUA negotiated 100rel
/// termination internally. Advisory until the subject narrows to non-DUT peer
/// binds or the rule models the B2BUA's internal PRACK-termination policy.
pub struct ReliableNeedsClientOptInRule;

impl CrossMessageAuditRule for ReliableNeedsClientOptInRule {
    fn name(&self) -> &'static str {
        "rfc3262.reliableNeedsClientOptIn"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uas])
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
                // branch → did the received INVITE opt into 100rel?
                let mut opt_in: HashMap<String, bool> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind == EventKind::Received {
                        if let SipMessage::Request(r) = msg {
                            if r.method.as_str().eq_ignore_ascii_case("INVITE") {
                                let branch = branch_of(msg);
                                if !branch.is_empty() {
                                    opt_in.entry(branch).or_insert_with(|| {
                                        has_option_tag(msg, "supported", "100rel")
                                            || has_option_tag(msg, "require", "100rel")
                                    });
                                }
                            }
                        }
                        continue;
                    }
                    if !is_reliable_1xx(msg) {
                        continue;
                    }
                    let branch = branch_of(msg);
                    if branch.is_empty() {
                        continue;
                    }
                    match opt_in.get(&branch) {
                        // INVITE never observed, or it did opt in → no finding.
                        None | Some(true) => continue,
                        Some(false) => out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Sent reliable 1xx (status {}, callId {}, branch {branch}) — \
                                 matching INVITE neither Supported:100rel nor Require:100rel \
                                 (RFC 3262 §3 / RFC3262-MUST-004)",
                                status(msg),
                                call_id(msg),
                            ),
                        )),
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.noReliable1xxOnInDialog  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-005) — no reliable 1xx to an in-dialog request.** A UAS
/// MUST NOT send a reliable 1xx (`Require:100rel`) in response to a request that
/// carries a To-tag (a mid-dialog re-INVITE etc.). A real UAS scopes the PRACK
/// machinery to the dialog-creating INVITE; the test UAS will reliably-18x
/// anything, so a re-INVITE answered with a reliable 18x only surfaces in the
/// recording.
pub struct NoReliable1xxOnInDialogRule;

impl CrossMessageAuditRule for NoReliable1xxOnInDialogRule {
    fn name(&self) -> &'static str {
        "rfc3262.noReliable1xxOnInDialog"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // branch → (in-dialog?, method) of the received request.
                let mut requests: HashMap<String, (bool, String)> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind == EventKind::Received {
                        if let SipMessage::Request(r) = msg {
                            let branch = branch_of(msg);
                            if !branch.is_empty() {
                                requests.entry(branch).or_insert_with(|| {
                                    (r.to.tag.is_some(), r.method.as_str().to_uppercase())
                                });
                            }
                        }
                        continue;
                    }
                    if !matches!(msg, SipMessage::Response(_))
                        || status(msg) <= 100
                        || status(msg) >= 200
                        || !has_option_tag(msg, "require", "100rel")
                    {
                        continue;
                    }
                    let branch = branch_of(msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let Some((in_dialog, method)) = requests.get(&branch) else {
                        continue;
                    };
                    if !*in_dialog {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Sent reliable 1xx on in-dialog request (status {}, callId {}, \
                             branch {branch}, method {method}) — forbidden per RFC 3262 §3 / \
                             RFC3262-MUST-005",
                            status(msg),
                            call_id(msg),
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.unmatchedPrackProxied  {Proxy}  [ADVISORY]
// ===========================================================================

/// **RFC 3262 §3 (MUST-006) — a proxy forwards an unmatched PRACK.** A proxy
/// receiving a PRACK that matches no locally-known reliable 1xx MUST forward it,
/// not absorb it. Observed loosely per-slot: a received PRACK whose RAck
/// response-num matches no received reliable 1xx RSeq in this slot, and for which
/// no outbound PRACK with the same RAck triple was seen, reads as "absorbed".
///
/// **Advisory** (TS `severityOverride:"advisory"`): the B2BUA worker terminates
/// PRACK per leg (not a §3 proxy). The peer's PRACK lands on the worker's bind in
/// dialog A's slice but the reliable 1xx that triggered it was emitted on dialog
/// B's leg (different Call-ID after the leg rewrite), so from a per-slice view the
/// PRACK appears "unmatched". Advisory until the subject narrows to a dedicated
/// proxy bind or the rule correlates across leg-mate slices.
pub struct UnmatchedPrackProxiedRule;

impl CrossMessageAuditRule for UnmatchedPrackProxiedRule {
    fn name(&self) -> &'static str {
        "rfc3262.unmatchedPrackProxied"
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
                if slot_is_relay(slot) {
                    continue;
                }
                let mut received_reliable_rseqs: HashSet<u64> = HashSet::new();
                // RAck triple (response-num, cseq-num, method) of sent PRACKs.
                let mut sent_prack_racks: HashSet<(u64, u64, String)> = HashSet::new();
                let mut received_pracks: Vec<(u64, u64, String, String, String)> = Vec::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind == EventKind::Received && matches!(msg, SipMessage::Response(_)) {
                        if status(msg) > 100 && status(msg) < 200 {
                            for n in rseq_values(msg) {
                                received_reliable_rseqs.insert(n);
                            }
                        }
                        continue;
                    }
                    if !is_prack(ev) {
                        continue;
                    }
                    let Some(rack) = rack_of(msg) else { continue };
                    let key = (rack.rseq, rack.seq, rack.method.to_uppercase());
                    if ev.kind == EventKind::Sent {
                        sent_prack_racks.insert(key);
                        continue;
                    }
                    received_pracks.push((
                        rack.rseq,
                        rack.seq,
                        rack.method.to_uppercase(),
                        call_id(msg).to_string(),
                        branch_of(msg),
                    ));
                }

                for (rseq, seq, method, cid, branch) in &received_pracks {
                    if received_reliable_rseqs.contains(rseq) {
                        continue;
                    }
                    if sent_prack_racks.contains(&(*rseq, *seq, method.clone())) {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "Received PRACK with RAck={rseq} {seq} {method} on proxy bind \
                             (callId {cid}, branch {branch}) — no matching reliable 1xx in this \
                             slot AND no outgoing PRACK observed (proxy must forward unmatched \
                             PRACKs, not absorb) — RFC 3262 §3 / RFC3262-MUST-006"
                        ),
                    ));
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.prackResponseSemantics  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-009/-010) — PRACK draws 2xx on a match, 481 otherwise.** A
/// UAS receiving a PRACK whose RAck response-num matches an unacked sent RSeq MUST
/// answer 2xx; with no match it MUST answer `481`. A real UAS keys this on its
/// live reliable-1xx state; the test UAS replies the same way to every PRACK, so
/// a mis-matched response only shows in the recording. The PRACK's own top-Via
/// branch correlates request to the UAS's response.
pub struct PrackResponseSemanticsRule;

impl CrossMessageAuditRule for PrackResponseSemanticsRule {
    fn name(&self) -> &'static str {
        "rfc3262.prackResponseSemantics"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callId → set of reliably-sent RSeqs (modelled "unacked" for the slot).
                let mut sent_rseqs: HashMap<String, HashSet<u64>> = HashMap::new();
                // PRACK branch → (callId, response-num, matched).
                let mut pending: HashMap<String, (String, u64, bool)> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Sent && is_reliable_1xx(msg) {
                        let cid = call_id(msg).to_string();
                        let set = sent_rseqs.entry(cid).or_default();
                        for n in rseq_values(msg) {
                            set.insert(n);
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Received && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        let cid = call_id(msg).to_string();
                        let matched =
                            sent_rseqs.get(&cid).is_some_and(|s| s.contains(&rack.rseq));
                        pending.insert(branch, (cid, rack.rseq, matched));
                        continue;
                    }

                    if ev.kind == EventKind::Sent
                        && matches!(msg, SipMessage::Response(_))
                        && cseq_method(msg).eq_ignore_ascii_case("PRACK")
                    {
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        let Some((cid, response_num, matched)) = pending.remove(&branch) else {
                            continue;
                        };
                        let st = status(msg);
                        if matched {
                            if (200..300).contains(&st) {
                                continue;
                            }
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Received PRACK with RAck.response-num {response_num} matches \
                                     sent RSeq, but agent responded {st} instead of 2xx (callId \
                                     {cid}) — RFC 3262 §3 / RFC3262-MUST-009"
                                ),
                            ));
                        } else {
                            if st == 481 {
                                continue;
                            }
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Received PRACK with RAck.response-num {response_num} matches \
                                     NO sent RSeq, but agent responded {st} instead of 481 (callId \
                                     {cid}) — RFC 3262 §3 / RFC3262-MUST-010"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.serialReliable1xx  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-012) — one unacked reliable 1xx at a time.** A UAS MUST NOT
/// send a second reliable 1xx in a dialog before the first is PRACKed. A real UAS
/// serialises its reliable provisionals on the PRACK; the test UAS fires whatever
/// the script schedules, so a raced pair only shows in the trace. A received PRACK
/// whose RAck response-num matches a queued RSeq retires it; identical-RSeq
/// retransmits are not "a second reliable 1xx".
pub struct SerialReliable1xxRule;

impl CrossMessageAuditRule for SerialReliable1xxRule {
    fn name(&self) -> &'static str {
        "rfc3262.serialReliable1xx"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callId → ordered list of unacked sent RSeqs.
                let mut unacked: HashMap<String, Vec<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Sent && is_reliable_1xx(msg) {
                        let cid = call_id(msg).to_string();
                        for n in rseq_values(msg) {
                            let list = unacked.entry(cid.clone()).or_default();
                            if list.contains(&n) {
                                continue; // retransmit
                            }
                            if let Some(&prior) = list.first() {
                                out.push((
                                    slot.bind_key.clone(),
                                    format!(
                                        "Sent second reliable 1xx (status {}, RSeq={n}, callId \
                                         {cid}) before prior RSeq {prior} PRACKed — RFC 3262 §3 / \
                                         RFC3262-MUST-012",
                                        status(msg),
                                    ),
                                ));
                            }
                            list.push(n);
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Received && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let cid = call_id(msg).to_string();
                        if let Some(list) = unacked.get_mut(&cid) {
                            if let Some(idx) = list.iter().position(|&n| n == rack.rseq) {
                                list.remove(idx);
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.rseqMonotonic  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-013) — RSeq increments by exactly one.** Each subsequent
/// reliable 1xx in a dialog MUST carry `RSeq = prior + 1`; RSeq never wraps. A
/// real UAS allocates RSeq contiguously off its dialog state; the test UAS emits
/// whatever the fixture hard-codes, so a gap / backwards move is only caught in
/// the recording. Identical-RSeq retransmits are skipped.
pub struct RseqMonotonicRule;

impl CrossMessageAuditRule for RseqMonotonicRule {
    fn name(&self) -> &'static str {
        "rfc3262.rseqMonotonic"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callId → prior sent RSeq.
                let mut prior_rseq: HashMap<String, u64> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind != EventKind::Sent || !is_reliable_1xx(msg) {
                        continue;
                    }
                    let cid = call_id(msg).to_string();
                    for n in rseq_values(msg) {
                        match prior_rseq.get(&cid).copied() {
                            None => {
                                prior_rseq.insert(cid.clone(), n);
                            }
                            Some(prior) if n == prior => {} // retransmit
                            Some(prior) if n == prior + 1 => {
                                prior_rseq.insert(cid.clone(), n);
                            }
                            Some(prior) => {
                                out.push((
                                    slot.bind_key.clone(),
                                    format!(
                                        "Sent reliable 1xx RSeq={n} not contiguous with prior \
                                         RSeq={prior} (callId {cid}) — RFC 3262 §3 / \
                                         RFC3262-MUST-013"
                                    ),
                                ));
                                prior_rseq.insert(cid.clone(), n);
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.delay2xxOnUnackedReliable1xxWithSdp  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-014, also restates MUST-028) — hold the 2xx until SDP is
/// PRACKed.** If a reliable 1xx carrying SDP is still unacked, the UAS MUST NOT
/// send the 2xx INVITE final until the PRACK arrives. A real UAS blocks the final
/// on the outstanding PRACK; the test UAS sends the 200 on schedule, so a premature
/// final only surfaces in the trace. Body-presence is the conservative proxy for
/// "carries SDP". Partitioned by `(Call-ID, INVITE branch)`; a PRACK clears its
/// matching RSeq from every same-Call-ID partition (PRACK's own branch differs).
pub struct Delay2xxOnUnackedReliable1xxWithSdpRule;

impl CrossMessageAuditRule for Delay2xxOnUnackedReliable1xxWithSdpRule {
    fn name(&self) -> &'static str {
        "rfc3262.delay2xxOnUnackedReliable1xxWithSdp"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // "callId\0branch" → unacked reliable-1xx-with-SDP RSeqs.
                let mut unacked: HashMap<String, HashSet<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Sent
                        && is_reliable_1xx(msg)
                        && !body_of(msg).is_empty()
                    {
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        let key = format!("{}\x00{branch}", call_id(msg));
                        let set = unacked.entry(key).or_default();
                        for n in rseq_values(msg) {
                            set.insert(n);
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Received && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let prefix = format!("{}\x00", call_id(msg));
                        for (key, set) in unacked.iter_mut() {
                            if key.starts_with(&prefix) {
                                set.remove(&rack.rseq);
                            }
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Sent
                        && matches!(msg, SipMessage::Response(_))
                        && cseq_method(msg).eq_ignore_ascii_case("INVITE")
                        && (200..300).contains(&status(msg))
                    {
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        let cid = call_id(msg).to_string();
                        let key = format!("{cid}\x00{branch}");
                        let Some(set) = unacked.get(&key) else { continue };
                        for rseq in set {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent 2xx INVITE response while reliable 1xx (RSeq={rseq}) with \
                                     SDP still unacked (callId {cid}, branch {branch}) — RFC 3262 \
                                     §3 / RFC3262-MUST-014"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.prackAcceptedAfterFinal  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-015) — accept a late PRACK with a 2xx.** A PRACK arriving
/// after the final INVITE response was sent still draws a 2xx — the UAS must
/// process PRACKs for outstanding reliable 1xx even once the call is
/// established/terminated. A real UAS keeps the PRACK server transaction alive;
/// the test UAS may reject a late PRACK, which only shows in the trace. Correlated
/// by the PRACK's own top-Via branch.
pub struct PrackAcceptedAfterFinalRule;

impl CrossMessageAuditRule for PrackAcceptedAfterFinalRule {
    fn name(&self) -> &'static str {
        "rfc3262.prackAcceptedAfterFinal"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callIds for which a final (≥200) INVITE response was sent.
                let mut final_sent: HashSet<String> = HashSet::new();
                // PRACK branch → callId, for PRACKs received after the final.
                let mut pending: HashMap<String, String> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Sent
                        && matches!(msg, SipMessage::Response(_))
                        && cseq_method(msg).eq_ignore_ascii_case("INVITE")
                        && status(msg) >= 200
                    {
                        final_sent.insert(call_id(msg).to_string());
                        continue;
                    }

                    if ev.kind == EventKind::Received && is_prack(ev) {
                        let cid = call_id(msg).to_string();
                        if !final_sent.contains(&cid) {
                            continue;
                        }
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        pending.insert(branch, cid);
                        continue;
                    }

                    if ev.kind == EventKind::Sent
                        && matches!(msg, SipMessage::Response(_))
                        && cseq_method(msg).eq_ignore_ascii_case("PRACK")
                    {
                        let branch = branch_of(msg);
                        if branch.is_empty() {
                            continue;
                        }
                        let Some(cid) = pending.remove(&branch) else { continue };
                        let st = status(msg);
                        if (200..300).contains(&st) {
                            continue;
                        }
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Received PRACK after final INVITE response was sent (callId {cid}, \
                                 PRACK branch {branch}) but PRACK got {st} instead of 2xx — RFC \
                                 3262 §3 / RFC3262-MUST-015"
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.noNewReliable1xxAfterFinal  {Uas}
// ===========================================================================

/// **RFC 3262 §3 (MUST-016) — no new reliable 1xx after the final.** A UAS MUST
/// NOT send a *new* reliable 1xx (unseen RSeq) on an INVITE transaction once a
/// final response (≥200) has been sent; retransmits of an already-emitted RSeq are
/// fine. A real UAS stops provisioning at the final; the test UAS may emit a stray
/// 18x, caught only in the recording. Partitioned by `(Call-ID, INVITE branch)`.
pub struct NoNewReliable1xxAfterFinalRule;

impl CrossMessageAuditRule for NoNewReliable1xxAfterFinalRule {
    fn name(&self) -> &'static str {
        "rfc3262.noNewReliable1xxAfterFinal"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let mut final_sent: HashSet<String> = HashSet::new();
                let mut seen_rseqs: HashMap<String, HashSet<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;
                    if ev.kind != EventKind::Sent
                        || !matches!(msg, SipMessage::Response(_))
                        || !cseq_method(msg).eq_ignore_ascii_case("INVITE")
                    {
                        continue;
                    }
                    let branch = branch_of(msg);
                    if branch.is_empty() {
                        continue;
                    }
                    let cid = call_id(msg).to_string();
                    let key = format!("{cid}\x00{branch}");

                    if status(msg) >= 200 {
                        final_sent.insert(key);
                        continue;
                    }
                    if status(msg) <= 100 || status(msg) >= 200 {
                        continue;
                    }
                    if !has_option_tag(msg, "require", "100rel") {
                        continue;
                    }
                    for n in rseq_values(msg) {
                        let seen = seen_rseqs.entry(key.clone()).or_default();
                        if seen.contains(&n) {
                            continue; // retransmit
                        }
                        if final_sent.contains(&key) {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent new reliable 1xx (RSeq={n}, callId {cid}) after final \
                                     INVITE response — forbidden per RFC 3262 §3 / RFC3262-MUST-016"
                                ),
                            ));
                        }
                        seen.insert(n);
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.uacIgnore100rel100Trying  {Uac}
// ===========================================================================

/// **RFC 3262 §4 (MUST-019) — never PRACK a 100 Trying.** If a `100 Trying`
/// response carries `Require:100rel`, the UAC MUST ignore the 100rel and MUST NOT
/// PRACK it — 100 Trying is never reliable. A real UAC's transaction layer
/// absorbs 100 Trying; the test UAC could be coaxed into PRACKing a malformed one,
/// which the recording catches. Keyed per Call-ID by the bogus 100's RSeq.
pub struct UacIgnore100rel100TryingRule;

impl CrossMessageAuditRule for UacIgnore100rel100TryingRule {
    fn name(&self) -> &'static str {
        "rfc3262.uacIgnore100rel100Trying"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callId → RSeqs seen on received 100 Trying carrying Require:100rel.
                let mut bogus_rseqs: HashMap<String, HashSet<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Received
                        && matches!(msg, SipMessage::Response(_))
                        && status(msg) == 100
                        && has_option_tag(msg, "require", "100rel")
                    {
                        let cid = call_id(msg).to_string();
                        let set = bogus_rseqs.entry(cid).or_default();
                        for n in rseq_values(msg) {
                            set.insert(n);
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Sent && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let cid = call_id(msg).to_string();
                        if bogus_rseqs.get(&cid).is_some_and(|s| s.contains(&rack.rseq)) {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent PRACK references RSeq {} from a received 100 Trying \
                                     carrying Require:100rel — UAC MUST ignore 100rel on 100 \
                                     Trying (RFC 3262 §4 / RFC3262-MUST-019)",
                                    rack.rseq,
                                ),
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.prackOnReliable1xx  {Uac}
// ===========================================================================

/// **RFC 3262 §4 (MUST-021) — every reliable 1xx draws a PRACK.** A UAC receiving
/// a reliable 1xx MUST send a matching PRACK (RAck response-num = the 1xx's RSeq).
/// Complement of the rack-correlation direction (every sent PRACK must match a
/// received reliable 1xx). A real UAC PRACKs each reliable provisional; the test
/// UAC may drop one, caught only in the recording. Per Call-ID, each received
/// reliable-1xx RSeq is a candidate retired by a matching sent PRACK; survivors
/// are flagged.
pub struct PrackOnReliable1xxRule;

impl CrossMessageAuditRule for PrackOnReliable1xxRule {
    fn name(&self) -> &'static str {
        "rfc3262.prackOnReliable1xx"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // callId → (RSeq → (status, pracked)).
                let mut candidates: HashMap<String, HashMap<u64, (u16, bool)>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Received
                        && matches!(msg, SipMessage::Response(_))
                        && status(msg) > 100
                        && status(msg) < 200
                        && has_option_tag(msg, "require", "100rel")
                    {
                        let cid = call_id(msg).to_string();
                        let inner = candidates.entry(cid).or_default();
                        for n in rseq_values(msg) {
                            inner.entry(n).or_insert((status(msg), false));
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Sent && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let cid = call_id(msg).to_string();
                        if let Some(inner) = candidates.get_mut(&cid) {
                            if let Some(c) = inner.get_mut(&rack.rseq) {
                                c.1 = true;
                            }
                        }
                    }
                }

                for (cid, inner) in &candidates {
                    for (rseq, (st, pracked)) in inner {
                        if *pracked {
                            continue;
                        }
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Received reliable 1xx (status {st}, RSeq={rseq}, callId {cid}) — \
                                 UAC did not send a matching PRACK (RFC 3262 §4 / RFC3262-MUST-021)"
                            ),
                        ));
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.uacRseqStrictness  {Uac}
// ===========================================================================

/// **RFC 3262 §4 (MUST-024) — PRACK only the in-order RSeq.** A UAC MUST PRACK
/// reliable 1xx in RSeq order; an out-of-order reliable 1xx MUST NOT be PRACKed
/// until the gap is filled. A real UAC buffers on the next-expected RSeq; the test
/// UAC may PRACK out of order, caught only in the trace. Per Call-ID, the first
/// received reliable 1xx seeds the expected value (then `+1` per in-order
/// arrival); a sent PRACK referencing a tracked out-of-order RSeq fires.
pub struct UacRseqStrictnessRule;

impl CrossMessageAuditRule for UacRseqStrictnessRule {
    fn name(&self) -> &'static str {
        "rfc3262.uacRseqStrictness"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let mut expected: HashMap<String, u64> = HashMap::new();
                let mut out_of_order: HashMap<String, HashSet<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if ev.kind == EventKind::Received
                        && matches!(msg, SipMessage::Response(_))
                        && status(msg) > 100
                        && status(msg) < 200
                        && has_option_tag(msg, "require", "100rel")
                    {
                        let cid = call_id(msg).to_string();
                        for n in rseq_values(msg) {
                            match expected.get(&cid).copied() {
                                None => {
                                    expected.insert(cid.clone(), n + 1);
                                }
                                Some(exp) if n == exp => {
                                    expected.insert(cid.clone(), n + 1);
                                }
                                Some(exp) if n == exp.wrapping_sub(1) => {} // retransmit of prior
                                Some(_) => {
                                    out_of_order.entry(cid.clone()).or_default().insert(n);
                                }
                            }
                        }
                        continue;
                    }

                    if ev.kind == EventKind::Sent && is_prack(ev) {
                        let Some(rack) = rack_of(msg) else { continue };
                        let cid = call_id(msg).to_string();
                        if out_of_order.get(&cid).is_some_and(|s| s.contains(&rack.rseq)) {
                            let exp = expected
                                .get(&cid)
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "?".to_string());
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent PRACK for out-of-order RSeq={} (expected {exp}, callId \
                                     {cid}) — UAC must PRACK in order (RFC 3262 §4 / \
                                     RFC3262-MUST-024)",
                                    rack.rseq,
                                ),
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// ===========================================================================
// rfc3262.prackOfferAnswerModel  {Uas}/{Uac}  [ADVISORY]
// ===========================================================================

/// **RFC 3262 §5 (MUST-025/-026/-027) — PRACK carries the answer to a 1xx offer.**
/// When a reliable 1xx carries an SDP **offer**, the corresponding PRACK MUST
/// carry the SDP answer; the 2xx to the PRACK MAY then carry a further exchange.
/// Enforcing "offer-in-1xx ⇒ answer-in-PRACK" makes the negotiation observable.
///
/// Offer detection: a reliable-1xx body counts as an OFFER only when the INVITE
/// transaction it answers carried **no** body — when the INVITE carried the
/// offer, the 1xx body is the **answer** (RFC 3264 §5) and the PRACK owes no
/// body at all. The old body-presence-only heuristic flagged every
/// INVITE-with-offer + reliable-183-with-answer + bodiless-PRACK flow (the
/// standard PRACK call setup!) on every endpoint — the e2e false-positive
/// class. An INVITE that was never observed in the slot conservatively keeps
/// the old reading (body ⇒ offer).
///
/// **Advisory** (TS `severityOverride:"advisory"`): the B2BUA terminates PRACK per
/// leg — a genuine reliable-1xx offer and its PRACK answer can still straddle
/// leg-mate slices (different Call-ID after the leg rewrite). Advisory until
/// the planned `_offer-answer.ts` helper models cross-leg PRACK O/A correlation.
pub struct PrackOfferAnswerModelRule;

impl CrossMessageAuditRule for PrackOfferAnswerModelRule {
    fn name(&self) -> &'static str {
        "rfc3262.prackOfferAnswerModel"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uac, UaRole::Uas])
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
                // callId → the INVITE (sent OR received) carried a body, i.e.
                // the offer rode the INVITE and any 1xx body is the answer.
                let mut invite_had_offer: HashMap<String, bool> = HashMap::new();
                // callId → RSeqs of reliable 1xx (sent OR received) that carried a body.
                let mut offer_rseqs: HashMap<String, HashSet<u64>> = HashMap::new();

                for ev in &slot.ordered {
                    let msg = &ev.msg;

                    if let SipMessage::Request(r) = msg {
                        if r.method.as_str().eq_ignore_ascii_case("INVITE") {
                            let had = invite_had_offer
                                .entry(call_id(msg).to_string())
                                .or_insert(false);
                            *had = *had || !body_of(msg).is_empty();
                            continue;
                        }
                    }

                    if is_reliable_1xx(msg) && !body_of(msg).is_empty() {
                        let cid = call_id(msg).to_string();
                        // The INVITE carried the offer ⇒ this body is the answer.
                        if invite_had_offer.get(&cid).copied().unwrap_or(false) {
                            continue;
                        }
                        let set = offer_rseqs.entry(cid).or_default();
                        for n in rseq_values(msg) {
                            set.insert(n);
                        }
                        continue;
                    }

                    if !is_prack(ev) {
                        continue;
                    }
                    let Some(rack) = rack_of(msg) else { continue };
                    let cid = call_id(msg).to_string();
                    if !offer_rseqs.get(&cid).is_some_and(|s| s.contains(&rack.rseq)) {
                        continue;
                    }
                    if !body_of(msg).is_empty() {
                        continue;
                    }
                    out.push((
                        slot.bind_key.clone(),
                        format!(
                            "PRACK for reliable-1xx-with-offer (RSeq={}, callId {cid}) carries no \
                             body — RFC 3262 §5 / RFC3262-MUST-025",
                            rack.rseq,
                        ),
                    ));
                }
            }
        }
        out
    }
}

/// The message body bytes (empty when absent). Mirrors the TS `msg.body.byteLength`
/// presence test used by the offer/answer-flavoured rules.
fn body_of(msg: &SipMessage) -> &[u8] {
    match msg {
        SipMessage::Request(r) => &r.body,
        SipMessage::Response(r) => &r.body,
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(RequireReliable1xxOnRequireRule),
        Arc::new(ReliableNeedsClientOptInRule),
        Arc::new(NoReliable1xxOnInDialogRule),
        Arc::new(UnmatchedPrackProxiedRule),
        Arc::new(PrackResponseSemanticsRule),
        Arc::new(SerialReliable1xxRule),
        Arc::new(RseqMonotonicRule),
        Arc::new(Delay2xxOnUnackedReliable1xxWithSdpRule),
        Arc::new(PrackAcceptedAfterFinalRule),
        Arc::new(NoNewReliable1xxAfterFinalRule),
        Arc::new(UacIgnore100rel100TryingRule),
        Arc::new(PrackOnReliable1xxRule),
        Arc::new(UacRseqStrictnessRule),
        Arc::new(PrackOfferAnswerModelRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    // -- event byte-builders (copied from the cross_generic / txn_correlation
    //    exemplars, extended with RFC 3262 headers + an optional SDP body) -----

    fn sent(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: "127.0.0.1:5070".parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:5091".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    const SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                       m=audio 5004 RTP/AVP 0\r\n";

    /// INVITE with caller-controlled `Require`/`Supported` option tags and To-tag.
    fn invite(branch: &str, extra: &str, to_tag: Option<&str>) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        format!(
            "INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n{extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// INVITE response, optionally reliable (`Require:100rel` + `RSeq`) and/or with SDP.
    fn invite_resp(
        status: u16,
        branch: &str,
        rseq: Option<u64>,
        require_100rel: bool,
        unsupported_100rel: bool,
        with_sdp: bool,
    ) -> Vec<u8> {
        let mut extra = String::new();
        if require_100rel {
            extra.push_str("Require: 100rel\r\n");
        }
        if unsupported_100rel {
            extra.push_str("Unsupported: 100rel\r\n");
        }
        if let Some(n) = rseq {
            extra.push_str(&format!("RSeq: {n}\r\n"));
        }
        let (body, clen) = if with_sdp {
            (SDP, SDP.len())
        } else {
            ("", 0)
        };
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n{extra}\
             Content-Length: {clen}\r\n\r\n{body}"
        )
        .into_bytes()
    }

    /// PRACK with caller-controlled RAck and optional SDP body.
    fn prack(branch: &str, rack: &str, with_sdp: bool) -> Vec<u8> {
        let (body, clen) = if with_sdp {
            (SDP, SDP.len())
        } else {
            ("", 0)
        };
        format!(
            "PRACK sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: {clen}\r\n\r\n{body}"
        )
        .into_bytes()
    }

    /// PRACK response with caller-controlled status on a chosen branch.
    fn prack_resp(status: u16, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    // ---- requireReliable1xxOnRequire ----------------------------------------

    #[test]
    fn require_reliable_1xx_clean_when_reliable() {
        // INVITE Require:100rel received; UAS answers reliable 180 + final.
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "Require: 100rel\r\n", None), 0),
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 2),
        ];
        assert!(RequireReliable1xxOnRequireRule.check(&evs).is_empty());
    }

    #[test]
    fn require_reliable_1xx_flagged_when_unreliable() {
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "Require: 100rel\r\n", None), 0),
            // Plain 180 — no Require:100rel/RSeq, and no 420.
            sent("uas", invite_resp(180, "z9hG4bK-i", None, false, false, false), 1),
        ];
        let f = RequireReliable1xxOnRequireRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-001/-002"), "{}", f[0].1);
    }

    #[test]
    fn require_reliable_1xx_clean_when_420() {
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "Require: 100rel\r\n", None), 0),
            sent("uas", invite_resp(420, "z9hG4bK-i", None, false, true, false), 1),
        ];
        assert!(RequireReliable1xxOnRequireRule.check(&evs).is_empty());
    }

    // ---- reliableNeedsClientOptIn -------------------------------------------

    #[test]
    fn reliable_opt_in_clean_when_supported() {
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
        ];
        assert!(ReliableNeedsClientOptInRule.check(&evs).is_empty());
    }

    #[test]
    fn reliable_opt_in_flagged_without_consent() {
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "", None), 0),
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
        ];
        let f = ReliableNeedsClientOptInRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-004"), "{}", f[0].1);
        assert!(ReliableNeedsClientOptInRule.force_advisory());
    }

    // ---- noReliable1xxOnInDialog --------------------------------------------

    #[test]
    fn no_reliable_1xx_in_dialog_clean_initial() {
        // No To-tag on the received INVITE → dialog-creating → fine.
        let evs = vec![
            recv("uas", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
        ];
        assert!(NoReliable1xxOnInDialogRule.check(&evs).is_empty());
    }

    #[test]
    fn no_reliable_1xx_in_dialog_flagged_on_retarget() {
        // Received re-INVITE carries a To-tag (in-dialog) → reliable 18x forbidden.
        let evs = vec![
            recv("uas", invite("z9hG4bK-re", "Supported: 100rel\r\n", Some("bt")), 0),
            sent("uas", invite_resp(180, "z9hG4bK-re", Some(1), true, false, false), 1),
        ];
        let f = NoReliable1xxOnInDialogRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-005"), "{}", f[0].1);
    }

    // ---- unmatchedPrackProxied ----------------------------------------------

    #[test]
    fn unmatched_prack_clean_when_matched() {
        // Received reliable 180 (RSeq 1) then received PRACK RAck 1 1 INVITE → matched.
        let evs = vec![
            recv("px", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            recv("px", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
        ];
        assert!(UnmatchedPrackProxiedRule.check(&evs).is_empty());
    }

    #[test]
    fn unmatched_prack_flagged_when_absorbed() {
        // PRACK RAck 9 references no observed reliable-1xx RSeq, no outgoing PRACK.
        let evs = vec![
            recv("px", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            recv("px", prack("z9hG4bK-p", "9 1 INVITE", false), 1),
        ];
        let f = UnmatchedPrackProxiedRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-006"), "{}", f[0].1);
        assert!(UnmatchedPrackProxiedRule.force_advisory());
    }

    // ---- prackResponseSemantics ---------------------------------------------

    #[test]
    fn prack_response_semantics_clean() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
            sent("uas", prack_resp(200, "z9hG4bK-p"), 2),
        ];
        assert!(PrackResponseSemanticsRule.check(&evs).is_empty());
    }

    #[test]
    fn prack_response_semantics_flagged_matched_non2xx() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
            // Matched RSeq but answered 481.
            sent("uas", prack_resp(481, "z9hG4bK-p"), 2),
        ];
        let f = PrackResponseSemanticsRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-009"), "{}", f[0].1);
    }

    #[test]
    fn prack_response_semantics_flagged_unmatched_non481() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            // RAck 9 matches nothing → should be 481.
            recv("uas", prack("z9hG4bK-p", "9 1 INVITE", false), 1),
            sent("uas", prack_resp(200, "z9hG4bK-p"), 2),
        ];
        let f = PrackResponseSemanticsRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-010"), "{}", f[0].1);
    }

    // ---- serialReliable1xx --------------------------------------------------

    #[test]
    fn serial_reliable_1xx_clean_after_prack() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(2), true, false, false), 2),
        ];
        assert!(SerialReliable1xxRule.check(&evs).is_empty());
    }

    #[test]
    fn serial_reliable_1xx_flagged_without_prack() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            // Second reliable 1xx before RSeq 1 PRACKed.
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(2), true, false, false), 1),
        ];
        let f = SerialReliable1xxRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-012"), "{}", f[0].1);
    }

    // ---- rseqMonotonic ------------------------------------------------------

    #[test]
    fn rseq_monotonic_clean_contiguous() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(5), true, false, false), 0),
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(6), true, false, false), 1),
        ];
        assert!(RseqMonotonicRule.check(&evs).is_empty());
    }

    #[test]
    fn rseq_monotonic_flagged_on_gap() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(5), true, false, false), 0),
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(8), true, false, false), 1),
        ];
        let f = RseqMonotonicRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-013"), "{}", f[0].1);
    }

    // ---- delay2xxOnUnackedReliable1xxWithSdp --------------------------------

    #[test]
    fn delay_2xx_clean_when_pracked() {
        let evs = vec![
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", true), 1),
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 2),
        ];
        assert!(Delay2xxOnUnackedReliable1xxWithSdpRule.check(&evs).is_empty());
    }

    #[test]
    fn delay_2xx_flagged_when_unacked() {
        let evs = vec![
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 0),
            // 200 before the reliable-1xx-with-SDP is PRACKed.
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 1),
        ];
        let f = Delay2xxOnUnackedReliable1xxWithSdpRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-014"), "{}", f[0].1);
    }

    // ---- prackAcceptedAfterFinal --------------------------------------------

    #[test]
    fn prack_after_final_clean_on_2xx() {
        let evs = vec![
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
            sent("uas", prack_resp(200, "z9hG4bK-p"), 2),
        ];
        assert!(PrackAcceptedAfterFinalRule.check(&evs).is_empty());
    }

    #[test]
    fn prack_after_final_flagged_on_reject() {
        let evs = vec![
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 0),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
            sent("uas", prack_resp(481, "z9hG4bK-p"), 2),
        ];
        let f = PrackAcceptedAfterFinalRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-015"), "{}", f[0].1);
    }

    // ---- noNewReliable1xxAfterFinal -----------------------------------------

    #[test]
    fn no_new_reliable_1xx_after_final_clean() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 1),
        ];
        assert!(NoNewReliable1xxAfterFinalRule.check(&evs).is_empty());
    }

    #[test]
    fn no_new_reliable_1xx_after_final_flagged() {
        let evs = vec![
            sent("uas", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            sent("uas", invite_resp(200, "z9hG4bK-i", None, false, false, false), 1),
            // Stray new reliable 18x after the final.
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(2), true, false, false), 2),
        ];
        let f = NoNewReliable1xxAfterFinalRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-016"), "{}", f[0].1);
    }

    // ---- uacIgnore100rel100Trying -------------------------------------------

    #[test]
    fn uac_ignore_100rel_clean_no_prack() {
        // Bogus 100 Trying with Require:100rel — but the UAC never PRACKs it.
        let evs = vec![
            sent("uac", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            recv("uac", invite_resp(100, "z9hG4bK-i", Some(1), true, false, false), 1),
        ];
        assert!(UacIgnore100rel100TryingRule.check(&evs).is_empty());
    }

    #[test]
    fn uac_ignore_100rel_flagged_on_prack() {
        let evs = vec![
            sent("uac", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            recv("uac", invite_resp(100, "z9hG4bK-i", Some(1), true, false, false), 1),
            // UAC wrongly PRACKs the 100.
            sent("uac", prack("z9hG4bK-p", "1 1 INVITE", false), 2),
        ];
        let f = UacIgnore100rel100TryingRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-019"), "{}", f[0].1);
    }

    // ---- prackOnReliable1xx -------------------------------------------------

    #[test]
    fn prack_on_reliable_1xx_clean() {
        let evs = vec![
            sent("uac", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            recv("uac", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
            sent("uac", prack("z9hG4bK-p", "1 1 INVITE", false), 2),
        ];
        assert!(PrackOnReliable1xxRule.check(&evs).is_empty());
    }

    #[test]
    fn prack_on_reliable_1xx_flagged_when_missing() {
        let evs = vec![
            sent("uac", invite("z9hG4bK-i", "Supported: 100rel\r\n", None), 0),
            // Reliable 180 received but never PRACKed.
            recv("uac", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 1),
        ];
        let f = PrackOnReliable1xxRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-021"), "{}", f[0].1);
    }

    // ---- uacRseqStrictness --------------------------------------------------

    #[test]
    fn uac_rseq_strictness_clean_in_order() {
        let evs = vec![
            recv("uac", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            sent("uac", prack("z9hG4bK-p1", "1 1 INVITE", false), 1),
            recv("uac", invite_resp(183, "z9hG4bK-i", Some(2), true, false, false), 2),
            sent("uac", prack("z9hG4bK-p2", "2 1 INVITE", false), 3),
        ];
        assert!(UacRseqStrictnessRule.check(&evs).is_empty());
    }

    #[test]
    fn uac_rseq_strictness_flagged_out_of_order() {
        let evs = vec![
            recv("uac", invite_resp(180, "z9hG4bK-i", Some(1), true, false, false), 0),
            // Out-of-order reliable 1xx (RSeq 5, expected 2) then PRACKed.
            recv("uac", invite_resp(183, "z9hG4bK-i", Some(5), true, false, false), 1),
            sent("uac", prack("z9hG4bK-p", "5 1 INVITE", false), 2),
        ];
        let f = UacRseqStrictnessRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-024"), "{}", f[0].1);
    }

    // ---- prackOfferAnswerModel ----------------------------------------------

    #[test]
    fn prack_offer_answer_clean_with_answer() {
        let evs = vec![
            recv("uac", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 0),
            // PRACK carries the answer body.
            sent("uac", prack("z9hG4bK-p", "1 1 INVITE", true), 1),
        ];
        assert!(PrackOfferAnswerModelRule.check(&evs).is_empty());
    }

    #[test]
    fn prack_offer_answer_flagged_without_answer() {
        let evs = vec![
            recv("uac", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 0),
            // Reliable 1xx had an offer but PRACK has no body.
            sent("uac", prack("z9hG4bK-p", "1 1 INVITE", false), 1),
        ];
        let f = PrackOfferAnswerModelRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-025"), "{}", f[0].1);
        assert!(PrackOfferAnswerModelRule.force_advisory());
    }

    #[test]
    fn prack_offer_answer_clean_when_invite_carried_the_offer() {
        // The STANDARD reliable-provisional call setup: INVITE carries the
        // offer, the reliable 183's SDP is the ANSWER (RFC 3264 §5), so the
        // PRACK legitimately has no body. The old body-presence heuristic
        // flagged this on every endpoint of every PRACK flow.
        let invite_with_offer = format!(
            "INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: {}\r\n\r\n{SDP}",
            SDP.len()
        )
        .into_bytes();
        // UAC view: sent INVITE w/ offer, received reliable 183 w/ answer,
        // sent bodiless PRACK.
        let evs = vec![
            sent("uac", invite_with_offer.clone(), 0),
            recv("uac", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 1),
            sent("uac", prack("z9hG4bK-p", "1 1 INVITE", false), 2),
        ];
        assert!(
            PrackOfferAnswerModelRule.check(&evs).is_empty(),
            "{:?}",
            PrackOfferAnswerModelRule.check(&evs)
        );
        // UAS view: received INVITE w/ offer, sent reliable 183 w/ answer,
        // received bodiless PRACK.
        let evs = vec![
            recv("uas", invite_with_offer, 0),
            sent("uas", invite_resp(183, "z9hG4bK-i", Some(1), true, false, true), 1),
            recv("uas", prack("z9hG4bK-p", "1 1 INVITE", false), 2),
        ];
        assert!(
            PrackOfferAnswerModelRule.check(&evs).is_empty(),
            "{:?}",
            PrackOfferAnswerModelRule.check(&evs)
        );
    }

    // ---- subject narrowing ---------------------------------------------------

    #[test]
    fn rfc3262_rule_subjects_narrowed() {
        assert_eq!(
            ReliableNeedsClientOptInRule.subject(),
            HashSet::from([UaRole::Uas])
        );
        assert_eq!(
            UnmatchedPrackProxiedRule.subject(),
            HashSet::from([UaRole::Proxy])
        );
        assert_eq!(
            PrackOfferAnswerModelRule.subject(),
            HashSet::from([UaRole::Uac, UaRole::Uas])
        );
    }
}
