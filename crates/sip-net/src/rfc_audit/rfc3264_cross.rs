//! Port of `tests/harness/rules/rfc/rfc3264-cross-message-rules.ts` — the
//! RFC 3264 offer/answer-model cross-message rules.
//!
//! Each rule walks the per-dialog projection ([`project_per_dialog`]) and, for
//! every agent slot, walks its ordered (sent + received) event stream pairing an
//! SDP **offer** (the first SDP body in the slot) with its **answer** (the next
//! SDP body in the opposite direction). Bodies are parsed with the shared
//! [`parse_sdp_body`] helper — the rules never re-parse SDP themselves.
//!
//! Per-UA dialog rules MUST skip relay slots ([`slot_is_relay`]): a transparent
//! proxy carries both directions of one Call-ID and would false-positive an
//! offer/answer pairing.
//!
//! Three rules are forced advisory (`force_advisory() -> true`): a B2BUA anchors
//! media and rewrites SDP across legs, so the per-slice (single-leg) view cannot
//! cleanly distinguish a genuine offer/answer violation from legitimate
//! cross-leg media anchoring. The justifications are copied from the TS advisory
//! override table.

use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    project_per_dialog, slot_is_relay, top_via_branch, AgentSlot, EventKind, OrderedEvent,
};
use crate::rfc_audit::offer_answer::{
    extract_direction, extract_rtpmaps, parse_sdp_body, SdpDirection,
};
use sip_message::SipMessage;

/// The raw message body bytes, regardless of request/response variant. The
/// offer/answer rules inspect `msg.body` directly (mirrors the TS `msg.body`).
fn msg_body(msg: &SipMessage) -> &[u8] {
    match msg {
        SipMessage::Request(r) => &r.body,
        SipMessage::Response(r) => &r.body,
    }
}

/// The top-`Via` `branch=` token for an event's message, or `""` (the TS
/// `msg.getHeader("via")[0]?.branch ?? ""`).
fn ev_branch(ev: &OrderedEvent) -> String {
    top_via_branch(&ev.msg).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// rfc3264.noNewOfferWhileOfferPending  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3264 §5 — a UA MUST NOT send a new SDP offer while an offer is
/// pending.** RFC3264-MUST-001 (no new offer while the *peer's* offer is
/// unanswered) + RFC3264-MUST-002 (no new offer while the UA's *own* prior
/// offer is unanswered). A real UA serialises offer/answer rounds and rejects a
/// glaring re-offer; the test UA silently accepts a second offer, masking a
/// glare bug. Per-slice tracking maintains `pendingOffer { side, branch }`: the
/// first SDP body is the offer; the answer is the next body in the opposite
/// direction whose top-Via branch matches the pending offer's branch.
pub struct NoNewOfferWhileOfferPendingRule;

impl CrossMessageAuditRule for NoNewOfferWhileOfferPendingRule {
    fn name(&self) -> &'static str {
        "rfc3264.noNewOfferWhileOfferPending"
    }

    /// Advisory: a B2BUA can legitimately emit a new offer on one leg before the
    /// prior offer's answer is observed on the same leg (the answer arrives on
    /// the other leg's slice after Call-ID rewrite). The per-slice pendingOffer
    /// tracker has no cross-leg view. Advisory until the subject narrows to
    /// non-DUT peer binds or the rule models cross-leg O/A correlation.
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
                // (side, branch) of the outstanding offer, if any.
                let mut pending: Option<(EventKind, String)> = None;

                for ev in &slot.ordered {
                    let body = msg_body(&ev.msg);
                    if body.is_empty() {
                        continue;
                    }
                    if parse_sdp_body(body).is_none() {
                        continue;
                    }
                    let branch = ev_branch(ev);

                    if let Some((pside, pbranch)) = pending.clone() {
                        // Answer on the same transaction (opposite direction,
                        // matching branch) clears the pending offer.
                        if !branch.is_empty() && branch == pbranch && ev.kind != pside {
                            pending = None;
                            continue;
                        }

                        if ev.kind == EventKind::Sent && pside == EventKind::Sent {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Sent SDP offer while prior offer (branch {pbranch}) still \
                                     pending (callId {}) — RFC 3264 §5 / RFC3264-MUST-002",
                                    slice.call_id,
                                ),
                            ));
                            pending = Some((EventKind::Sent, branch));
                            continue;
                        }
                        if ev.kind == EventKind::Received && pside == EventKind::Received {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Received SDP offer while prior offer (branch {pbranch}) still \
                                     pending (callId {}) — RFC 3264 §5 / RFC3264-MUST-001",
                                    slice.call_id,
                                ),
                            ));
                            pending = Some((EventKind::Received, branch));
                            continue;
                        }
                        // Cross-direction body on a different branch: treat as a
                        // new offer round (conservative — the prior round's
                        // answer didn't show up in this slot).
                        pending = Some((ev.kind, branch));
                        continue;
                    }

                    pending = Some((ev.kind, branch));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Offer/answer pairing shared by the "first answer vs offer" rules.
// ---------------------------------------------------------------------------

/// Walk a slot's ordered stream, find the first SDP body (the **offer**) and the
/// next SDP body in the *opposite* direction (the **answer**), and invoke
/// `judge(offer_doc, answer_doc)` once on that pair. Stops after the first
/// answer. Bodies in the same direction as the offer (re-offers / retransmits)
/// are skipped while waiting for the answer. SKIPs (no callback) when no offer,
/// or no opposite-direction answer, is observed in the slot.
fn with_first_offer_answer<F>(slot: &AgentSlot, mut judge: F)
where
    F: FnMut(&crate::rfc_audit::offer_answer::SdpDoc, &crate::rfc_audit::offer_answer::SdpDoc),
{
    let mut offer: Option<(EventKind, crate::rfc_audit::offer_answer::SdpDoc)> = None;
    for ev in &slot.ordered {
        let body = msg_body(&ev.msg);
        if body.is_empty() {
            continue;
        }
        let Some(sdp) = parse_sdp_body(body) else {
            continue;
        };
        match &offer {
            None => {
                offer = Some((ev.kind, sdp));
            }
            Some((oside, odoc)) => {
                if ev.kind == *oside {
                    continue;
                }
                judge(odoc, &sdp);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// rfc3264.answerMLineCountMatchesOffer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — the first answer's `m=` line count MUST equal the offer's.**
/// RFC3264-MUST-018: the answer has exactly one `m=` line per offered stream
/// (rejected streams keep their slot with port 0, never disappear). A real UA's
/// stack rejects a count mismatch; the test UA accepts add/drop, masking a
/// stream-table desync.
pub struct AnswerMLineCountMatchesOfferRule;

impl CrossMessageAuditRule for AnswerMLineCountMatchesOfferRule {
    fn name(&self) -> &'static str {
        "rfc3264.answerMLineCountMatchesOffer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                with_first_offer_answer(slot, |offer, answer| {
                    if answer.media.len() != offer.media.len() {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Answer SDP m= count ({}) differs from offer m= count ({}) \
                                 (callId {}) — RFC 3264 §6 / RFC3264-MUST-018",
                                answer.media.len(),
                                offer.media.len(),
                                slice.call_id,
                            ),
                        ));
                    }
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.answerTLineEqualsOffer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — the answer's `t=` line MUST equal the offer's.**
/// RFC3264-MUST-019: the session time bounds are preserved verbatim across the
/// offer/answer exchange. A divergent `t=` desyncs the two ends' notion of when
/// the session is active; a real UA preserves it, the test UA accepts a rewrite.
pub struct AnswerTLineEqualsOfferRule;

impl CrossMessageAuditRule for AnswerTLineEqualsOfferRule {
    fn name(&self) -> &'static str {
        "rfc3264.answerTLineEqualsOffer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                with_first_offer_answer(slot, |offer, answer| {
                    if answer.t_line != offer.t_line {
                        out.push((
                            slot.bind_key.clone(),
                            format!(
                                "Answer SDP t= line ('{}') differs from offer t= line ('{}') \
                                 (callId {}) — RFC 3264 §6 / RFC3264-MUST-019",
                                answer.t_line.as_deref().unwrap_or(""),
                                offer.t_line.as_deref().unwrap_or(""),
                                slice.call_id,
                            ),
                        ));
                    }
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.answerMediaTypeMatchesOffer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6.1 — answer media type MUST match the offer's at each index.**
/// RFC3264-MUST-022: per-stream-index pairing (audio↔audio, video↔video). A
/// swapped media type at a shared index breaks the per-stream correlation both
/// ends rely on; a real UA enforces the pairing, the test UA accepts a swap.
pub struct AnswerMediaTypeMatchesOfferRule;

impl CrossMessageAuditRule for AnswerMediaTypeMatchesOfferRule {
    fn name(&self) -> &'static str {
        "rfc3264.answerMediaTypeMatchesOffer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                with_first_offer_answer(slot, |offer, answer| {
                    let shared = offer.media.len().min(answer.media.len());
                    for i in 0..shared {
                        let offer_type = &offer.media[i].r#type;
                        let answer_type = &answer.media[i].r#type;
                        if offer_type != answer_type {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Answer m=[{i}] media type '{answer_type}' does not match offer \
                                     m=[{i}] type '{offer_type}' (callId {}) — \
                                     RFC 3264 §6.1 / RFC3264-MUST-022",
                                    slice.call_id,
                                ),
                            ));
                        }
                    }
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.directionPairValid  [ADVISORY]
// ---------------------------------------------------------------------------

/// The set of answer directions valid for a given offer direction (RFC 3264
/// §6.1 matrix): `sendonly→{recvonly,inactive}`, `recvonly→{sendonly,inactive}`,
/// `inactive→{inactive}`, `sendrecv→{sendrecv,sendonly,recvonly,inactive}`.
fn answer_valid_for_offer(offer: SdpDirection, answer: SdpDirection) -> bool {
    use SdpDirection::*;
    match offer {
        SendOnly => matches!(answer, RecvOnly | Inactive),
        RecvOnly => matches!(answer, SendOnly | Inactive),
        Inactive => matches!(answer, Inactive),
        SendRecv => matches!(answer, SendRecv | SendOnly | RecvOnly | Inactive),
    }
}

/// **RFC 3264 §6.1 — per-stream direction pairing MUST follow the answer
/// matrix.** RFC3264-MUST-023: an answer to `sendonly` is `recvonly`/`inactive`,
/// etc. A mis-paired direction (e.g. `sendonly→sendonly`) leaves both ends
/// trying to send with neither receiving; a real UA enforces the matrix, the
/// test UA accepts the impossible pairing.
pub struct DirectionPairValidRule;

impl CrossMessageAuditRule for DirectionPairValidRule {
    fn name(&self) -> &'static str {
        "rfc3264.directionPairValid"
    }

    /// Advisory: a B2BUA may translate SDP direction attributes across legs as
    /// policy (e.g. force `sendrecv` on one leg even when the peer offered
    /// `inactive` for hold on the other leg). The per-slice direction pairing
    /// cannot distinguish policy translation from a genuine violation. Advisory
    /// until the subject narrows to non-DUT peer binds.
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
                with_first_offer_answer(slot, |offer, answer| {
                    let shared = offer.media.len().min(answer.media.len());
                    for i in 0..shared {
                        let offer_dir = extract_direction(&offer.media[i]);
                        let answer_dir = extract_direction(&answer.media[i]);
                        if !answer_valid_for_offer(offer_dir, answer_dir) {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Answer direction '{}' invalid for offer direction '{}' at \
                                     m=[{i}] (callId {}) — RFC 3264 §6.1 / RFC3264-MUST-023",
                                    dir_str(answer_dir),
                                    dir_str(offer_dir),
                                    slice.call_id,
                                ),
                            ));
                        }
                    }
                });
            }
        }
        out
    }
}

/// Lower-case wire spelling of a direction (matches the TS attribute tokens).
fn dir_str(d: SdpDirection) -> &'static str {
    match d {
        SdpDirection::SendRecv => "sendrecv",
        SdpDirection::SendOnly => "sendonly",
        SdpDirection::RecvOnly => "recvonly",
        SdpDirection::Inactive => "inactive",
    }
}

// ---------------------------------------------------------------------------
// rfc3264.rejectedStreamMinimalAnswer
// ---------------------------------------------------------------------------

/// **RFC 3264 §6 — a rejected answer stream (port 0) MUST still list a format.**
/// RFC3264-MUST-021: even when disabling a stream the answer keeps at least one
/// media format token on the `m=` line (a bare `m=audio 0 RTP/AVP` is illegal).
/// A real UA emits the minimal format; the test UA accepts the bare rejection.
pub struct RejectedStreamMinimalAnswerRule;

impl CrossMessageAuditRule for RejectedStreamMinimalAnswerRule {
    fn name(&self) -> &'static str {
        "rfc3264.rejectedStreamMinimalAnswer"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                with_first_offer_answer(slot, |_offer, answer| {
                    for (i, m) in answer.media.iter().enumerate() {
                        if m.port != Some(0) {
                            continue;
                        }
                        if m.formats.is_empty() {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Rejected stream m=[{i}] (port=0) carries no media format \
                                     tokens (callId {}) — RFC 3264 §6 / RFC3264-MUST-021",
                                    slice.call_id,
                                ),
                            ));
                        }
                    }
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.reOfferMLineCountMonotonic
// ---------------------------------------------------------------------------

/// **RFC 3264 §8 — re-offer `m=` count MUST NOT decrease.** RFC3264-MUST-042 +
/// RFC3264-MUST-043: a removed stream keeps its `m=` slot (port 0), never
/// vanishes, and new streams appear *below* existing ones. A count decrease
/// means a stream was dropped instead of disabled, desyncing the stream table;
/// a real UA keeps the slot, the test UA accepts the shrink. Tracks every *sent*
/// SDP body's `m=` count and fires on any decrease between consecutive offers.
pub struct ReOfferMLineCountMonotonicRule;

impl CrossMessageAuditRule for ReOfferMLineCountMonotonicRule {
    fn name(&self) -> &'static str {
        "rfc3264.reOfferMLineCountMonotonic"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                let mut prev_count: Option<usize> = None;
                for ev in &slot.ordered {
                    if ev.kind != EventKind::Sent {
                        continue;
                    }
                    let body = msg_body(&ev.msg);
                    if body.is_empty() {
                        continue;
                    }
                    let Some(sdp) = parse_sdp_body(body) else {
                        continue;
                    };
                    let count = sdp.media.len();
                    if let Some(prev) = prev_count {
                        if count < prev {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Re-offer m= count {count} decreased from prior offer {prev} \
                                     — streams must keep their slot (port=0) (callId {}) — \
                                     RFC 3264 §8 / RFC3264-MUST-042",
                                    slice.call_id,
                                ),
                            ));
                        }
                    }
                    prev_count = Some(count);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.zeroPortPropagation  [ADVISORY]
// ---------------------------------------------------------------------------

/// **RFC 3264 §8 — a stream offered with port 0 MUST be port 0 in the answer.**
/// RFC3264-MUST-044: a disabled stream stays disabled — the answer cannot
/// resurrect it with a live port. A real UA propagates the zero port; the test
/// UA accepts a phantom port assignment.
pub struct ZeroPortPropagationRule;

impl CrossMessageAuditRule for ZeroPortPropagationRule {
    fn name(&self) -> &'static str {
        "rfc3264.zeroPortPropagation"
    }

    /// Advisory: a B2BUA anchors media per leg and assigns its own RTP ports —
    /// a peer-side offer with port=0 (stream disabled) becomes a B2BUA-side
    /// offer/answer with the B2BUA's anchored port. The per-slice view cannot
    /// see the cross-leg port rewrite. Advisory until the subject narrows to
    /// non-DUT peer binds or the rule models B2BUA media anchoring.
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
                with_first_offer_answer(slot, |offer, answer| {
                    let shared = offer.media.len().min(answer.media.len());
                    for i in 0..shared {
                        if offer.media[i].port != Some(0) {
                            continue;
                        }
                        let answer_port = answer.media[i].port;
                        // `None` (unparseable answer port) cannot be judged — SKIP.
                        let Some(p) = answer_port else { continue };
                        if p != 0 {
                            out.push((
                                slot.bind_key.clone(),
                                format!(
                                    "Offer m=[{i}] has port=0 but answer m=[{i}] has port={p} \
                                     (callId {}) — RFC 3264 §8 / RFC3264-MUST-044",
                                    slice.call_id,
                                ),
                            ));
                        }
                    }
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// rfc3264.payloadTypeMappingStable
// ---------------------------------------------------------------------------

/// **RFC 3264 §8.3.2 — the dynamic payload-type → codec mapping MUST be stable
/// across SDP versions.** RFC3264-MUST-047: once `a=rtpmap:<pt> <enc>` binds a
/// PT in a session, a later SDP version MUST NOT rebind that PT to a different
/// encoding (the peer caches the mapping). A real UA keeps the binding fixed;
/// the test UA accepts a silent remap, corrupting media decode. Tracks the first
/// encoding seen per PT across every SDP body in the slot and fires on a remap.
pub struct PayloadTypeMappingStableRule;

impl CrossMessageAuditRule for PayloadTypeMappingStableRule {
    fn name(&self) -> &'static str {
        "rfc3264.payloadTypeMappingStable"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        let mut out = Vec::new();
        for slice in project_per_dialog(events) {
            for slot in &slice.per_agent {
                if slot_is_relay(slot) {
                    continue;
                }
                // PT → first-seen encoding (insertion-stable, small).
                let mut seen: Vec<(String, String)> = Vec::new();
                for ev in &slot.ordered {
                    let body = msg_body(&ev.msg);
                    if body.is_empty() {
                        continue;
                    }
                    let Some(sdp) = parse_sdp_body(body) else {
                        continue;
                    };
                    for media in &sdp.media {
                        for (pt, enc) in extract_rtpmaps(media) {
                            match seen.iter().find(|(p, _)| *p == pt) {
                                None => seen.push((pt, enc)),
                                Some((_, prev)) if *prev != enc => {
                                    out.push((
                                        slot.bind_key.clone(),
                                        format!(
                                            "Dynamic payload-type {pt} remapped: was '{prev}' now \
                                             '{enc}' (callId {}) — \
                                             RFC 3264 §8.3.2 / RFC3264-MUST-047",
                                            slice.call_id,
                                        ),
                                    ));
                                }
                                Some(_) => {}
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The cross-message rules defined in this module. Aggregated by [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![
        Arc::new(NoNewOfferWhileOfferPendingRule),
        Arc::new(AnswerMLineCountMatchesOfferRule),
        Arc::new(AnswerTLineEqualsOfferRule),
        Arc::new(AnswerMediaTypeMatchesOfferRule),
        Arc::new(DirectionPairValidRule),
        Arc::new(RejectedStreamMinimalAnswerRule),
        Arc::new(ReOfferMLineCountMonotonicRule),
        Arc::new(ZeroPortPropagationRule),
        Arc::new(PayloadTypeMappingStableRule),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
                packet: UdpPacket { raw, src: src.parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    // An INVITE (UAC→UAS) carrying an SDP offer. `branch` keys the transaction.
    fn invite_with_sdp(branch: &str, sdp: &str) -> Vec<u8> {
        msg_with_body("INVITE", branch, 1, "sip:alice@127.0.0.1", None, sdp)
    }

    // An in-dialog re-INVITE (carries the confirmed To-tag so it lands in the
    // same dialog slice as the initial INVITE).
    fn reinvite_with_sdp(branch: &str, cseq: u32, sdp: &str) -> Vec<u8> {
        msg_with_body("INVITE", branch, cseq, "sip:alice@127.0.0.1", Some("bt"), sdp)
    }

    // The 200 OK (received by the UAC) carrying the SDP answer, same branch +
    // the remote tag that confirms the dialog.
    fn ok_200_with_sdp(branch: &str, sdp: &str) -> Vec<u8> {
        resp_with_body(200, branch, 1, "INVITE", "bt", sdp)
    }

    fn msg_with_body(
        method: &str,
        branch: &str,
        cseq: u32,
        from_uri: &str,
        ttag: Option<&str>,
        body: &str,
    ) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        let head = format!(
            "{method} sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{from_uri}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n",
            body.len(),
        );
        let mut v = head.into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    fn resp_with_body(
        status: u16,
        branch: &str,
        cseq: u32,
        method: &str,
        ttag: &str,
        body: &str,
    ) -> Vec<u8> {
        let head = format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag={ttag}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {}\r\n\r\n",
            body.len(),
        );
        let mut v = head.into_bytes();
        v.extend_from_slice(body.as_bytes());
        v
    }

    const OFFER_1AUDIO: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    const ANSWER_1AUDIO: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";

    // ---- noNewOfferWhileOfferPending -----------------------------------

    #[test]
    fn no_new_offer_clean_serialized_round() {
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        assert!(NoNewOfferWhileOfferPendingRule.check(&evs).is_empty());
    }

    #[test]
    fn no_new_offer_second_sent_offer_flagged() {
        // Two sent offers on different branches with no answer in between.
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            sent("alice", invite_with_sdp("z9hG4bK-j", OFFER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        let f = NoNewOfferWhileOfferPendingRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-002"), "{}", f[0].1);
    }

    // ---- answerMLineCountMatchesOffer ----------------------------------

    const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

    #[test]
    fn m_line_count_clean() {
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        assert!(AnswerMLineCountMatchesOfferRule.check(&evs).is_empty());
    }

    #[test]
    fn m_line_count_mismatch_flagged() {
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_2MEDIA), "127.0.0.1:5070", 1),
        ];
        let f = AnswerMLineCountMatchesOfferRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-018"), "{}", f[0].1);
    }

    // ---- answerTLineEqualsOffer ----------------------------------------

    #[test]
    fn t_line_clean() {
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        assert!(AnswerTLineEqualsOfferRule.check(&evs).is_empty());
    }

    #[test]
    fn t_line_mismatch_flagged() {
        let answer = ANSWER_1AUDIO.replace("t=0 0", "t=100 200");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", &answer), "127.0.0.1:5070", 1),
        ];
        let f = AnswerTLineEqualsOfferRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-019"), "{}", f[0].1);
    }

    // ---- answerMediaTypeMatchesOffer -----------------------------------

    #[test]
    fn media_type_clean() {
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        assert!(AnswerMediaTypeMatchesOfferRule.check(&evs).is_empty());
    }

    #[test]
    fn media_type_swap_flagged() {
        // Answer swaps audio→video at index 0.
        let answer = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", answer), "127.0.0.1:5070", 1),
        ];
        let f = AnswerMediaTypeMatchesOfferRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-022"), "{}", f[0].1);
    }

    // ---- directionPairValid --------------------------------------------

    #[test]
    fn direction_pair_clean() {
        // Offer sendrecv → answer sendrecv (valid).
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        assert!(DirectionPairValidRule.check(&evs).is_empty());
    }

    #[test]
    fn direction_pair_invalid_flagged() {
        // Offer sendonly → answer sendonly (invalid: must be recvonly|inactive).
        let offer = OFFER_1AUDIO.replace("a=sendrecv", "a=sendonly");
        let answer = ANSWER_1AUDIO.replace("a=sendrecv", "a=sendonly");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", &offer), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", &answer), "127.0.0.1:5070", 1),
        ];
        let f = DirectionPairValidRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-023"), "{}", f[0].1);
    }

    // ---- rejectedStreamMinimalAnswer -----------------------------------

    #[test]
    fn rejected_stream_with_format_clean() {
        // Answer rejects the stream (port 0) but keeps a format token.
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", &answer), "127.0.0.1:5070", 1),
        ];
        assert!(RejectedStreamMinimalAnswerRule.check(&evs).is_empty());
    }

    #[test]
    fn rejected_stream_bare_flagged() {
        // Answer rejects with a bare m= line — no format tokens.
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", &answer), "127.0.0.1:5070", 1),
        ];
        let f = RejectedStreamMinimalAnswerRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-021"), "{}", f[0].1);
    }

    // ---- reOfferMLineCountMonotonic ------------------------------------

    #[test]
    fn re_offer_count_monotonic_clean() {
        // Second sent offer keeps the same m= count (kept-with-port-0 form).
        let reoffer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
            sent("alice", reinvite_with_sdp("z9hG4bK-j", 2, &reoffer), "127.0.0.1:5070", 2),
        ];
        assert!(ReOfferMLineCountMonotonicRule.check(&evs).is_empty());
    }

    #[test]
    fn re_offer_count_decrease_flagged() {
        // First sent offer has 2 m= lines, the re-offer drops to 1.
        let two = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=sendrecv\r\n";
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", two), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
            sent("alice", reinvite_with_sdp("z9hG4bK-j", 2, OFFER_1AUDIO), "127.0.0.1:5070", 2),
        ];
        let f = ReOfferMLineCountMonotonicRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-042"), "{}", f[0].1);
    }

    // ---- zeroPortPropagation -------------------------------------------

    #[test]
    fn zero_port_propagation_clean() {
        // Offer disables the stream (port 0); answer keeps port 0.
        let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let answer = ANSWER_1AUDIO.replace("m=audio 50000 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", &offer), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", &answer), "127.0.0.1:5070", 1),
        ];
        assert!(ZeroPortPropagationRule.check(&evs).is_empty());
    }

    #[test]
    fn zero_port_propagation_phantom_flagged() {
        // Offer port 0; answer assigns a live port.
        let offer = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", &offer), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
        ];
        let f = ZeroPortPropagationRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-044"), "{}", f[0].1);
    }

    // ---- payloadTypeMappingStable --------------------------------------

    #[test]
    fn payload_type_mapping_stable_clean() {
        let reoffer = OFFER_1AUDIO.to_string();
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
            sent("alice", reinvite_with_sdp("z9hG4bK-j", 2, &reoffer), "127.0.0.1:5070", 2),
        ];
        assert!(PayloadTypeMappingStableRule.check(&evs).is_empty());
    }

    #[test]
    fn payload_type_remap_flagged() {
        // Re-offer rebinds PT 96 from opus to H264.
        let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
        let evs = vec![
            sent("alice", invite_with_sdp("z9hG4bK-i", OFFER_1AUDIO), "127.0.0.1:5070", 0),
            recv("alice", ok_200_with_sdp("z9hG4bK-i", ANSWER_1AUDIO), "127.0.0.1:5070", 1),
            sent("alice", reinvite_with_sdp("z9hG4bK-j", 2, &reoffer), "127.0.0.1:5070", 2),
        ];
        let f = PayloadTypeMappingStableRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].1.contains("MUST-047"), "{}", f[0].1);
    }
}
