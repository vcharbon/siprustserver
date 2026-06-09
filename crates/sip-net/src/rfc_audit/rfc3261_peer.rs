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
use sip_message::message_helpers::get_headers;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{PeerAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, extract_route_uri, msg_headers, route_is_loose, status, to_tag,
    top_via_branch,
};
use crate::rfc_audit::txn_correlation::split_option_tags;
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

/// True iff any Require / Proxy-Require value tokenises to at least one
/// non-empty option-tag. Mirrors the TS `hasOptionTag`.
fn carries_option_tag(values: &[&str]) -> bool {
    !split_option_tags(values.iter().copied()).is_empty()
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
                    let method = req.method.as_str();
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
                    let method = req.method.as_str();
                    if method != "CANCEL" && method != "ACK" {
                        continue;
                    }
                    let require = get_headers(msg_headers(&m), "require");
                    let proxy_require = get_headers(msg_headers(&m), "proxy-require");
                    if !carries_option_tag(&require) && !carries_option_tag(&proxy_require) {
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
/// CSeq method token is not `CANCEL` is a §9.1 violation. Factored out so the
/// defense-in-depth path can be exercised on a [`SipMessage`] built *outside*
/// the parser — the only way this rule ever fires, since the strict and lenient
/// parsers both reject a wire CANCEL whose request-method differs from its CSeq
/// method before the rule could see it (see the struct doc).
fn cancel_cseq_violation(m: &SipMessage) -> Option<String> {
    let SipMessage::Request(req) = m else {
        return None;
    };
    if req.method.as_str() != "CANCEL" {
        return None;
    }
    let cseq_m = cseq_method(m);
    if cseq_m == "CANCEL" {
        return None;
    }
    Some(format!(
        "CANCEL request carries CSeq method={cseq_m} (expected CANCEL) \
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
            let routes = get_headers(msg_headers(&m), "route");
            let Some(first) = routes.first() else {
                continue;
            };
            let first_uri = extract_route_uri(first);
            if route_is_loose(&first_uri) {
                continue;
            }
            out.push(format!(
                "Sent {} request still carries strict-route topmost Route entry — §16.6 \
                 step 6 swap may not have run (RFC3261-MUST-113)",
                req.method.as_str(),
            ));
        }
        out
    }
}

/// The peer rules defined in this module. Aggregated by [`super::rfc_peer_rules`].
pub(crate) fn peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    vec![
        Arc::new(NoToTagOnInitialRequestRule),
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
        // request-method differs from its CSeq method (extract_fields.rs), so
        // such a message can only arise from an internal builder that bypasses
        // field extraction — exactly the defense-in-depth path this rule guards.
        // We reproduce that path: parse a valid CANCEL, then mutate the parsed
        // CSeq method to INVITE (a builder bug), and assert the rule's
        // per-message decision flags it.
        let parser = super::super::lenient_parser();
        let mut m = parser.parse(&cancel_cseq("CANCEL")).expect("valid CANCEL parses");
        if let SipMessage::Request(req) = &mut m {
            req.cseq.method = sip_message::Method::Invite;
        }
        let detail = cancel_cseq_violation(&m).expect("CSeq method mismatch must be flagged");
        assert!(detail.contains("MUST-045"), "{detail}");
        assert!(detail.contains("method=INVITE"), "{detail}");
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
