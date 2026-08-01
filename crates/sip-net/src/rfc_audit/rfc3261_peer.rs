//! Port of `tests/harness/rules/rfc/rfc3261-peer-rules.ts` — per-message peer
//! rules introduced by Phase 2 of the RFC verification plan. Each
//! [`PeerAuditRule`] sees one bind's recorded events (sent + received), lenient-
//! parses the direction it judges, and returns violation detail strings; the
//! gate / ledger attaches the bind + severity.
//!
//! Authoring pattern follows the [`super::starter_peer`] exemplar: a unit struct
//! per rule, a `name()` of `rfc3261.<lowerCamel>` (the TS `rfc.<x>` id), a
//! `subject()` (default = all roles, narrowed only where the TS rule narrows),
//! and a `check()` over [`super::lenient_parser`]d bytes. Add each struct to
//! [`peer_rules`].

use std::collections::HashSet;
use std::sync::Arc;

use layer_harness::Stamped;
use sip_message::header::{ProxyRequire, Require};
use sip_message::{SipMessage, SipParser};

use crate::contracts::{PeerAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, route_entries, status, to_tag, top_via_branch, value_of,
};
use crate::types::UaRole;

/// Methods that can legitimately initiate a transaction *outside* a dialog.
/// Anything else (BYE, ACK, UPDATE, INFO, PRACK, CANCEL) is intrinsically
/// in-dialog and a To-tag is expected; firing on those would be noise (a
/// test-fixture artifact, not a real M-016 violation). Mirrors the TS
/// `DIALOG_INITIATING_METHODS`.
const DIALOG_INITIATING_METHODS: &[&str] = &[
    "INVITE",
    "REGISTER",
    "SUBSCRIBE",
    "OPTIONS",
    "REFER",
    "MESSAGE",
    "PUBLISH",
    "NOTIFY",
];

/// True iff the message names at least one option-tag on Require or
/// Proxy-Require.
fn carries_option_tag(m: &SipMessage) -> bool {
    value_of::<Require>(m).is_some_and(|set| !set.is_empty())
        || value_of::<ProxyRequire>(m).is_some_and(|set| !set.is_empty())
}

/// **RFC 3261 §8.1.1.2 — a request outside of a dialog MUST NOT carry a To
/// tag (RFC3261-MUST-016).** The To tag identifies the peer of an established
/// dialog; a dialog-initiating request has no peer yet, so a real UAC mints its
/// To with no tag. The test UAC, which fills whatever header set it is handed,
/// can leak a tag onto an initial request. Vantage heuristic: the first event
/// this bind sees for a given Call-ID is the dialog-initiating point from this
/// peer's side; if that first event is a SENT dialog-initiating REQUEST carrying
/// a To-tag, it is an initial request outside any dialog with a tag — a clear
/// violation. Once any traffic for the Call-ID has been observed (sent or
/// received), later same-Call-ID requests are in-dialog and a To-tag is
/// legitimate. CANCEL stays out of this lane — `rfc.tags` cross-correlates its
/// tag semantics against the matching INVITE.
pub struct NoToTagOnInitialRequestRule;

impl PeerAuditRule for NoToTagOnInitialRequestRule {
    fn name(&self) -> &'static str {
        "rfc3261.noToTagOnInitialRequest"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        let mut seen_call_ids: HashSet<String> = HashSet::new();
        for s in events {
            match &s.event {
                SignalingNetworkEvent::SendCalled { msg, .. } => {
                    let Ok(m) = parser.parse(msg) else {
                        continue;
                    };
                    let cid = call_id(&m);
                    if cid.is_empty() {
                        continue;
                    }
                    let first_for_call_id = seen_call_ids.insert(cid.to_string());
                    let SipMessage::Request(req) = &m else {
                        continue;
                    };
                    if !first_for_call_id {
                        continue;
                    }
                    let method = req.method().as_str();
                    if !DIALOG_INITIATING_METHODS.contains(&method) {
                        continue;
                    }
                    if let Some(tag) = to_tag(&m).filter(|t| !t.is_empty()) {
                        out.push(format!(
                            "{method} request outside any dialog carries To-tag={tag} \
                             (RFC 3261 §8.1.1.2 / RFC3261-MUST-016)"
                        ));
                    }
                }
                SignalingNetworkEvent::RecvItem { packet, .. } => {
                    let Ok(m) = parser.parse(&packet.raw) else {
                        continue;
                    };
                    let cid = call_id(&m);
                    if !cid.is_empty() {
                        seen_call_ids.insert(cid.to_string());
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// Per-Call-ID witness state for [`InDialogToTagRule`], built from everything
/// (sent + received) this bind carried for the Call-ID before the request under
/// judgement.
#[derive(Default)]
struct DialogWitness {
    /// A 2xx to INVITE carrying a To-tag was observed — the dialog is confirmed,
    /// so both its tags exist and every later request in it must reproduce them.
    confirmed: bool,
    /// Top-`Via` branches of To-tag-less INVITE requests: the dialog-establishing
    /// transaction, whose retransmissions stay legitimately tag-less even after
    /// the 2xx lands.
    establishing_branches: HashSet<String>,
    /// Top-`Via` branches on which a non-2xx final was observed. The ACK for such
    /// a final is generated by the client transaction on the INVITE's own branch
    /// and copies that response's To verbatim (§17.1.1.3), so its tag is the
    /// responder's business — `rfc3261.toTagPresence` judges that.
    non_2xx_branches: HashSet<String>,
    /// The establishing INVITE was seen in this direction.
    sent_establishing: bool,
    recv_establishing: bool,
}

impl DialogWitness {
    /// This bind carried the establishing INVITE in BOTH directions — a
    /// transparent relay for this Call-ID, forwarding a peer's headers unchanged.
    /// A missing tag is then the originating UA's finding, not the relay's.
    fn is_relay(&self) -> bool {
        self.sent_establishing && self.recv_establishing
    }

    /// The violation detail for a request this bind sends, or `None` when the
    /// request is out of dialog, echoes another message's To, or carries the tag.
    fn violation(&self, m: &SipMessage) -> Option<String> {
        let SipMessage::Request(req) = m else {
            return None;
        };
        if !self.confirmed || self.is_relay() {
            return None;
        }
        if to_tag(m).is_some_and(|t| !t.is_empty()) {
            return None;
        }
        let method = req.method().as_str();
        if method == "CANCEL" {
            return None;
        }
        let branch = top_via_branch(m);
        if branch.as_ref().is_some_and(|b| self.establishing_branches.contains(b)) {
            return None;
        }
        if method == "ACK" && branch.as_ref().is_some_and(|b| self.non_2xx_branches.contains(b)) {
            return None;
        }
        Some(format!(
            "{method} request inside a confirmed dialog carries no To-tag — RFC 3261 §12.2.1.1 \
             requires the dialog's remote tag (RFC3261-MUST-066); the peer cannot dialog-match it"
        ))
    }

    /// Fold one carried message into the witness.
    fn observe(&mut self, m: &SipMessage, sent: bool) {
        match m {
            SipMessage::Request(req) => {
                if req.method().as_str() != "INVITE" || to_tag(m).is_some_and(|t| !t.is_empty()) {
                    return;
                }
                if let Some(b) = top_via_branch(m) {
                    self.establishing_branches.insert(b);
                }
                if sent {
                    self.sent_establishing = true;
                } else {
                    self.recv_establishing = true;
                }
            }
            SipMessage::Response(_) => {
                let st = status(m);
                if cseq_method(m) == "INVITE"
                    && (200..300).contains(&st)
                    && to_tag(m).is_some_and(|t| !t.is_empty())
                {
                    self.confirmed = true;
                }
                if (300..700).contains(&st) {
                    if let Some(b) = top_via_branch(m) {
                        self.non_2xx_branches.insert(b);
                    }
                }
            }
        }
    }
}

/// **RFC 3261 §12.2.1.1 — a request sent within a dialog MUST carry the dialog's
/// remote tag in To (RFC3261-MUST-066).** The To-tag is half the dialog
/// identifier: a UAS receiving a tag-less in-dialog request cannot match it to
/// the dialog and answers 481. The sender mints the header from its own dialog
/// state, so this bind's **sent** requests are judged, on a Call-ID this bind has
/// watched reach a *confirmed* dialog (a To-tagged 2xx to INVITE — both tags then
/// exist, whichever side the request comes from).
///
/// The guards keep it to genuine sender defects: a request whose To is a verbatim
/// echo of another message is not the sender's to fill — CANCEL copies the INVITE
/// it cancels (§9.1) and the ACK for a non-2xx final copies that response
/// (§17.1.1.3, correlated by the INVITE branch it shares) — a retransmission of
/// the dialog-establishing INVITE (same branch) is still the tag-less initial
/// request `rfc3261.noToTagOnInitialRequest` governs, and a relay forwarding a
/// peer's headers unchanged reports nothing, so the finding lands on the UA that
/// wrote the header. Requests inside an *early* dialog are out of scope here.
pub struct InDialogToTagRule;

impl PeerAuditRule for InDialogToTagRule {
    fn name(&self) -> &'static str {
        "rfc3261.inDialogToTag"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uac, UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut per_call: std::collections::HashMap<String, DialogWitness> =
            std::collections::HashMap::new();
        let mut out = Vec::new();
        for s in events {
            let (raw, sent) = match &s.event {
                SignalingNetworkEvent::SendCalled { msg, .. } => (msg.as_slice(), true),
                SignalingNetworkEvent::RecvItem { packet, .. } => (packet.raw.as_slice(), false),
                _ => continue,
            };
            let Ok(m) = parser.parse(raw) else {
                continue;
            };
            let cid = call_id(&m);
            if cid.is_empty() {
                continue;
            }
            let witness = per_call.entry(cid.to_string()).or_default();
            if sent {
                out.extend(witness.violation(&m));
            }
            witness.observe(&m, sent);
        }
        out
    }
}

/// **RFC 3261 §8.2.2.3 — Require / Proxy-Require MUST NOT be used in a CANCEL,
/// or in an ACK for a non-2xx response (RFC3261-MUST-034).** Both are hop-by-hop
/// transaction-management requests that must not impose extension requirements;
/// a real UA omits Require/Proxy-Require on them. Sent CANCEL: any present option
/// tag is a violation. Sent ACK: only when it acknowledges a non-2xx final —
/// per §17.1.1.3 that ACK is generated by the client transaction and shares the
/// INVITE's top-Via branch + Call-ID, whereas a 2xx ACK is a fresh transaction
/// with a new branch. So the rule scans prior RECEIVED responses on this bind
/// for a `(Call-ID, branch)` match with status ∈ [300, 699]; if none is found
/// the ACK is presumed to be for a 2xx and skipped — keeping the rule
/// self-contained without dragging in cross-message correlation.
pub struct NoRequireOnCancelOrAckRule;

impl PeerAuditRule for NoRequireOnCancelOrAckRule {
    fn name(&self) -> &'static str {
        "rfc3261.noRequireOnCancelOrAck"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        // (Call-ID, branch) -> status for inbound responses on this bind.
        let mut received_response_status: std::collections::HashMap<String, u16> =
            std::collections::HashMap::new();
        let key = |cid: &str, branch: &str| format!("{cid}\x00{branch}");

        for s in events {
            match &s.event {
                SignalingNetworkEvent::RecvItem { packet, .. } => {
                    let Ok(m) = parser.parse(&packet.raw) else {
                        continue;
                    };
                    if !matches!(m, SipMessage::Response(_)) {
                        continue;
                    }
                    let cid = call_id(&m);
                    if cid.is_empty() {
                        continue;
                    }
                    let Some(branch) = top_via_branch(&m) else {
                        continue;
                    };
                    received_response_status.insert(key(cid, &branch), status(&m));
                }
                SignalingNetworkEvent::SendCalled { msg, .. } => {
                    let Ok(m) = parser.parse(msg) else {
                        continue;
                    };
                    let SipMessage::Request(req) = &m else {
                        continue;
                    };
                    let method = req.method().as_str();
                    if method != "CANCEL" && method != "ACK" {
                        continue;
                    }
                    if !carries_option_tag(&m) {
                        continue;
                    }
                    if method == "CANCEL" {
                        out.push(
                            "CANCEL request carries Require/Proxy-Require — forbidden by \
                             RFC 3261 §8.2.2.3 / RFC3261-MUST-034"
                                .to_string(),
                        );
                        continue;
                    }
                    // ACK branch: only flag when correlatable to a non-2xx response.
                    let cid = call_id(&m);
                    let Some(branch) = top_via_branch(&m) else {
                        continue;
                    };
                    if cid.is_empty() {
                        continue;
                    }
                    if let Some(&st) = received_response_status.get(&key(cid, &branch)) {
                        if (300..=699).contains(&st) {
                            out.push(format!(
                                "ACK for non-2xx response (status={st}) carries \
                                 Require/Proxy-Require — forbidden by RFC 3261 §8.2.2.3 / \
                                 RFC3261-MUST-034"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// **RFC 3261 §9.1 — the CSeq method part of a CANCEL MUST be `CANCEL`
/// (RFC3261-MUST-045).** A CANCEL reuses the CSeq *number* of the request it
/// cancels but carries its own method token; a real UA writes `CANCEL`. This is
/// defense-in-depth: the strict parser already rejects any wire message whose
/// request method differs from its CSeq method, so this only fires on a message
/// constructed outside the parser (an internal builder bypassing field
/// extraction). The CSeq number-equality aspect is owned by `rfc.cseq`.
pub struct CancelCseqMethodRule;

impl PeerAuditRule for CancelCseqMethodRule {
    fn name(&self) -> &'static str {
        "rfc3261.cancelCseqMethod"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for s in events {
            let SignalingNetworkEvent::SendCalled { msg, .. } = &s.event else {
                continue;
            };
            let Ok(m) = parser.parse(msg) else {
                continue;
            };
            if let Some(detail) = cancel_cseq_violation(&m) {
                out.push(detail);
            }
        }
        out
    }
}

/// The per-message decision for [`CancelCseqMethodRule`]: a sent CANCEL whose
/// CSeq method token is not `CANCEL` is a §9.1 violation.
fn cancel_cseq_violation(m: &SipMessage) -> Option<String> {
    let SipMessage::Request(req) = m else {
        return None;
    };
    cancel_cseq_mismatch(req.method().as_str(), cseq_method(m))
}

/// The decision itself, over the two method tokens. Factored out so the
/// defense-in-depth path stays testable: the strict and lenient parsers both
/// reject a wire CANCEL whose request-method differs from its CSeq method, and
/// a frozen message cannot be edited into that state, so no message carrying it
/// can be built (see the struct doc).
fn cancel_cseq_mismatch(method: &str, cseq_method: &str) -> Option<String> {
    if method != "CANCEL" || cseq_method == "CANCEL" {
        return None;
    }
    Some(format!(
        "CANCEL request carries CSeq method={cseq_method} (expected CANCEL) \
         — RFC 3261 §9.1 / RFC3261-MUST-045"
    ))
}

/// **RFC 3261 §16.6 step 6 — a proxy forwarding through a strict-route next hop
/// MUST swap the Request-URI with the topmost Route URI (RFC3261-MUST-113).**
/// When the route set's first URI lacks `;lr` (a strict route), §16.6 step 6.b
/// pushes the current Request-URI to the bottom of the Route list and lifts the
/// first Route URI into the Request-URI. A single outbound message can't replay
/// the pre-swap state, so the rule flags the structural indicator: an outbound
/// request whose topmost Route value is still strict-route (lacks `;lr`). That
/// either means the swap never ran (violation) or — rarely — the next hop is
/// itself a strict-route target that survives the swap; both are worth
/// surfacing. Subject is `{Proxy}` only per the manifest; ships regression-only
/// and the maintainer triages if it fires.
pub struct StrictRouteShuffleOnSendRule;

impl PeerAuditRule for StrictRouteShuffleOnSendRule {
    fn name(&self) -> &'static str {
        "rfc3261.strictRouteShuffleOnSend"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Proxy])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for s in events {
            let SignalingNetworkEvent::SendCalled { msg, .. } = &s.event else {
                continue;
            };
            let Ok(m) = parser.parse(msg) else {
                continue;
            };
            let SipMessage::Request(req) = &m else {
                continue;
            };
            let routes = route_entries(&m);
            let Some(first) = routes.first() else {
                continue;
            };
            if first.uri().is_loose_route() {
                continue;
            }
            out.push(format!(
                "Sent {} request still carries strict-route topmost Route entry — §16.6 \
                 step 6 swap may not have run (RFC3261-MUST-113)",
                req.method().as_str(),
            ));
        }
        out
    }
}

/// The peer rules defined in this module. Aggregated by [`super::rfc_peer_rules`].
pub(crate) fn peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    vec![
        Arc::new(NoToTagOnInitialRequestRule),
        Arc::new(InDialogToTagRule),
        Arc::new(NoRequireOnCancelOrAckRule),
        Arc::new(CancelCseqMethodRule),
        Arc::new(StrictRouteShuffleOnSendRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    fn sent_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: "127.0.0.1:5080".parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:9999".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    // ----- noToTagOnInitialRequest --------------------------------------

    fn invite_with_to(to_params: &str, call_id: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>{to_params}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn initial_invite_without_to_tag_is_clean() {
        let evs = vec![sent_at("alice", invite_with_to("", "cid-1@h"), 0)];
        assert!(NoToTagOnInitialRequestRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn initial_invite_with_to_tag_is_flagged() {
        let evs = vec![sent_at("alice", invite_with_to(";tag=btag", "cid-1@h"), 0)];
        let f = NoToTagOnInitialRequestRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-016"), "{}", f[0]);
    }

    #[test]
    fn in_dialog_request_with_to_tag_is_clean() {
        // First observe inbound traffic for the Call-ID, then a SENT request with
        // a To-tag is legitimately in-dialog.
        let evs = vec![
            recv_at("bob", invite_with_to("", "cid-2@h"), 0),
            sent_at("bob", invite_with_to(";tag=btag", "cid-2@h"), 1),
        ];
        assert!(NoToTagOnInitialRequestRule.check(&evs, "bob").is_empty());
    }

    // ----- inDialogToTag -------------------------------------------------

    /// A request whose To params (`""` = tag-less) and transaction identity are
    /// spelled by the caller, on the shared `cid-d@h` dialog.
    fn dialog_req(method: &str, branch: &str, cseq: u32, to_params: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>{to_params}\r\n\
             Call-ID: cid-d@h\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn dialog_resp(status: u16, cseq: u32, cseq_method: &str, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-d@h\r\n\
             CSeq: {cseq} {cseq_method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// INVITE → 200(tag=bt) → ACK, as the caller bind recorded it.
    fn confirmed_dialog() -> Vec<Stamped<SignalingNetworkEvent>> {
        vec![
            sent_at("alice", dialog_req("INVITE", "z9hG4bK-i", 1, ""), 0),
            recv_at("alice", dialog_resp(200, 1, "INVITE", "z9hG4bK-i"), 1),
            sent_at("alice", dialog_req("ACK", "z9hG4bK-k", 1, ";tag=bt"), 2),
        ]
    }

    #[test]
    fn tagless_bye_in_confirmed_dialog_is_flagged() {
        let mut evs = confirmed_dialog();
        evs.push(sent_at("alice", dialog_req("BYE", "z9hG4bK-b", 2, ""), 3));
        let f = InDialogToTagRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-066"), "{}", f[0]);
        assert!(f[0].starts_with("BYE"), "{}", f[0]);
    }

    #[test]
    fn compliant_bye_in_confirmed_dialog_is_clean() {
        let mut evs = confirmed_dialog();
        evs.push(sent_at("alice", dialog_req("BYE", "z9hG4bK-b", 2, ";tag=bt"), 3));
        assert!(InDialogToTagRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn tagless_2xx_ack_is_flagged() {
        // The 2xx ACK is a fresh transaction the sender builds from dialog state
        // — no response to echo, so the tag is its own to write.
        let evs = vec![
            sent_at("alice", dialog_req("INVITE", "z9hG4bK-i", 1, ""), 0),
            recv_at("alice", dialog_resp(200, 1, "INVITE", "z9hG4bK-i"), 1),
            sent_at("alice", dialog_req("ACK", "z9hG4bK-k", 1, ""), 2),
        ];
        let f = InDialogToTagRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].starts_with("ACK"), "{}", f[0]);
    }

    #[test]
    fn ack_for_a_non_2xx_re_invite_is_not_judged() {
        // The re-INVITE draws a 488; its ACK rides the INVITE branch and echoes
        // that response's To, so the tag is the responder's business.
        let mut evs = confirmed_dialog();
        evs.push(sent_at("alice", dialog_req("INVITE", "z9hG4bK-r", 2, ";tag=bt"), 3));
        evs.push(recv_at("alice", dialog_resp(488, 2, "INVITE", "z9hG4bK-r"), 4));
        evs.push(sent_at("alice", dialog_req("ACK", "z9hG4bK-r", 2, ""), 5));
        assert!(InDialogToTagRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn cancel_is_not_judged() {
        // CANCEL copies the To of the INVITE it cancels (§9.1).
        let mut evs = confirmed_dialog();
        evs.push(sent_at("alice", dialog_req("INVITE", "z9hG4bK-r", 2, ";tag=bt"), 3));
        evs.push(sent_at("alice", dialog_req("CANCEL", "z9hG4bK-r", 2, ""), 4));
        assert!(InDialogToTagRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn establishing_invite_retransmit_after_the_2xx_is_clean() {
        // Same branch as the initial INVITE: still the dialog-establishing
        // request, which is tag-less by §8.1.1.2.
        let mut evs = confirmed_dialog();
        evs.push(sent_at("alice", dialog_req("INVITE", "z9hG4bK-i", 1, ""), 3));
        assert!(InDialogToTagRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn out_of_dialog_request_is_clean() {
        // No confirmed dialog on the Call-ID — a tag-less request is correct.
        let evs = vec![
            sent_at("alice", invite_with_to("", "cid-o@h"), 0),
            sent_at("alice", dialog_req("OPTIONS", "z9hG4bK-o", 1, ""), 1),
        ];
        assert!(InDialogToTagRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn relay_forwarding_a_tagless_request_is_not_judged() {
        // The bind carried the establishing INVITE both ways: it forwards the
        // originator's headers, so the finding belongs to that originator.
        let evs = vec![
            recv_at("lb", dialog_req("INVITE", "z9hG4bK-i", 1, ""), 0),
            sent_at("lb", dialog_req("INVITE", "z9hG4bK-i2", 1, ""), 1),
            recv_at("lb", dialog_resp(200, 1, "INVITE", "z9hG4bK-i2"), 2),
            sent_at("lb", dialog_req("BYE", "z9hG4bK-b", 2, ""), 3),
        ];
        assert!(InDialogToTagRule.check(&evs, "lb").is_empty());
    }

    #[test]
    fn in_dialog_to_tag_subject_excludes_proxy() {
        assert_eq!(
            InDialogToTagRule.subject(),
            HashSet::from([UaRole::Uac, UaRole::Uas])
        );
    }

    // ----- noRequireOnCancelOrAck ---------------------------------------

    fn cancel(require: &str) -> Vec<u8> {
        format!(
            "CANCEL sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-inv\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-3@h\r\n\
             CSeq: 1 CANCEL\r\n\
             {require}\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn cancel_without_require_is_clean() {
        let evs = vec![sent_at("alice", cancel(""), 0)];
        assert!(NoRequireOnCancelOrAckRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn cancel_with_require_is_flagged() {
        let evs = vec![sent_at("alice", cancel("Require: 100rel\r\n"), 0)];
        let f = NoRequireOnCancelOrAckRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-034"), "{}", f[0]);
    }

    #[test]
    fn ack_for_non_2xx_with_require_is_flagged() {
        // A 486 arrives on branch z9hG4bK-ack, then an ACK with Require is sent on
        // the same Call-ID + branch (a non-2xx ACK shares the INVITE branch).
        let resp_486 = b"SIP/2.0 486 Busy Here\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-ack\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-4@h\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
            .to_vec();
        let ack = b"ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-ack\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-4@h\r\n\
             CSeq: 1 ACK\r\n\
             Proxy-Require: foo\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
            .to_vec();
        let evs = vec![recv_at("alice", resp_486, 0), sent_at("alice", ack, 1)];
        let f = NoRequireOnCancelOrAckRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("non-2xx"), "{}", f[0]);
    }

    // ----- cancelCseqMethod ---------------------------------------------

    fn cancel_cseq(cseq_method: &str) -> Vec<u8> {
        format!(
            "CANCEL sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-c\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-5@h\r\n\
             CSeq: 1 {cseq_method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn cancel_with_cancel_cseq_is_clean() {
        let evs = vec![sent_at("alice", cancel_cseq("CANCEL"), 0)];
        assert!(CancelCseqMethodRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn cancel_with_invite_cseq_is_flagged() {
        // The strict AND lenient parsers both reject a wire CANCEL whose
        // request-method differs from its CSeq method (extract_fields.rs), and a
        // frozen message cannot be edited into that state — so the rule is
        // exercised on the decision itself, which is the defense-in-depth path
        // it guards.
        let parser = super::super::lenient_parser();
        assert!(parser.parse(&cancel_cseq("INVITE")).is_err(), "the wire form is rejected");
        let detail = cancel_cseq_mismatch("CANCEL", "INVITE")
            .expect("CSeq method mismatch must be flagged");
        assert!(detail.contains("MUST-045"), "{detail}");
        assert!(detail.contains("method=INVITE"), "{detail}");
        // A well-formed CANCEL is silent, on both paths.
        assert!(cancel_cseq_mismatch("CANCEL", "CANCEL").is_none());
        let m = parser.parse(&cancel_cseq("CANCEL")).expect("valid CANCEL parses");
        assert!(cancel_cseq_violation(&m).is_none());
    }

    // ----- strictRouteShuffleOnSend -------------------------------------

    fn fwd_with_route(route: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-r\r\n\
             Route: {route}\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>\r\n\
             Call-ID: cid-6@h\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn loose_route_topmost_is_clean() {
        let evs = vec![sent_at("p", fwd_with_route("<sip:proxy@127.0.0.1;lr>"), 0)];
        assert!(StrictRouteShuffleOnSendRule.check(&evs, "p").is_empty());
    }

    #[test]
    fn strict_route_topmost_is_flagged() {
        let evs = vec![sent_at("p", fwd_with_route("<sip:proxy@127.0.0.1>"), 0)];
        let f = StrictRouteShuffleOnSendRule.check(&evs, "p");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-113"), "{}", f[0]);
    }

    #[test]
    fn strict_route_subject_is_proxy_only() {
        assert_eq!(
            StrictRouteShuffleOnSendRule.subject(),
            HashSet::from([UaRole::Proxy])
        );
    }
}
