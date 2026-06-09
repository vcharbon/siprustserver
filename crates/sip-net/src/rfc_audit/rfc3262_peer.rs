//! Port of `tests/harness/rules/rfc/rfc3262-peer-rules.ts` — per-message peer
//! rules for RFC 3262 (PRACK / 100rel). Each [`PeerAuditRule`] sees one bind's
//! recorded events (sent + received) and returns violation detail strings; the
//! gate / ledger attaches the bind + severity.
//!
//! Cross-message rules for RFC 3262 (offer/answer model, glare, RSeq
//! monotonicity) live in the cross-rule modules when they land.

use std::collections::HashSet;
use std::sync::Arc;

use layer_harness::Stamped;
use sip_message::message_helpers::get_headers;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{PeerAuditRule, SignalingNetworkEvent};
use crate::rfc_audit::dialog_model::{msg_headers, status};
use crate::rfc_audit::txn_correlation::split_option_tags;
use crate::types::UaRole;

/// `RSeq` MUST be in `[1, 2^31 - 1]` (RFC 3262 §3 / MUST-008).
const RSEQ_MAX: u64 = 2_147_483_647;

/// Iterate the messages this bind **sent** (`SendCalled`), lenient-parsed. The
/// sender mints the headers a peer rule judges (a `Require:` it adds, an `RSeq`
/// it stamps), so sent-direction is the right place to attribute the defect.
fn sent_messages<'a>(
    events: &'a [Stamped<SignalingNetworkEvent>],
    parser: &'a CustomParser,
) -> impl Iterator<Item = SipMessage> + 'a {
    events.iter().filter_map(move |s| match &s.event {
        SignalingNetworkEvent::SendCalled { msg, .. } => parser.parse(msg).ok(),
        _ => None,
    })
}

/// True iff any of `values` (comma-separated option-tag rows) lists `tag`.
fn has_option_tag(values: &[&str], tag: &str) -> bool {
    split_option_tags(values.iter().copied()).iter().any(|t| t == tag)
}

/// **RFC 3262 §4 — `Require: 100rel` MUST NOT appear on a non-INVITE request
/// (RFC3262-MUST-017).** "A Require header with the value 100rel MUST NOT be
/// present in any requests excepting INVITE." A reliable-provisional contract is
/// only meaningful for the INVITE transaction that PRACK acknowledges; a UA that
/// stamps `Require: 100rel` on, say, a BYE or OPTIONS forces a Bad-Extension
/// rejection at a strict peer. The test UA's lenient parser would let such a
/// request through unflagged, so this rule inspects every sent request and flags
/// a non-INVITE method carrying the `100rel` option tag in any `Require` row.
pub struct No100relRequireOnNonInviteRule;

impl PeerAuditRule for No100relRequireOnNonInviteRule {
    fn name(&self) -> &'static str {
        "rfc3262.no100relRequireOnNonInvite"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Request(req) = &msg else {
                continue;
            };
            if req.method.as_str() == "INVITE" {
                continue;
            }
            let require = get_headers(msg_headers(&msg), "require");
            if has_option_tag(&require, "100rel") {
                out.push(format!(
                    "{} request carries Require: 100rel — only INVITE may (RFC 3262 §4 / \
                     RFC3262-MUST-017)",
                    req.method.as_str(),
                ));
            }
        }
        out
    }
}

/// **RFC 3262 §3 — reliable-provisional header integrity (RFC3262-MUST-003 /
/// -007 / -008).** A UAS MUST NOT try to send 100 (Trying) reliably (MUST-003:
/// a sent 100 must carry neither `RSeq` nor `Require: 100rel`); a reliable
/// 1xx (101–199 signalled by `Require: 100rel`) MUST include an `RSeq`
/// (MUST-007) whose value sits in `[1, 2^31 - 1]` (MUST-008). A real UAC
/// PRACK-acks against that `RSeq`, so a missing/out-of-range value or a
/// reliable-100 breaks the PRACK matching outright; the test UAS's lenient
/// encoder would emit the malformed response silently, so this rule inspects
/// every sent 1xx response on the UAS's bind. `RSeq` is not a typed header, so
/// its numeric value is recovered from the raw row.
pub struct Reliable1xxHeadersRule;

impl PeerAuditRule for Reliable1xxHeadersRule {
    fn name(&self) -> &'static str {
        "rfc3262.reliable1xxHeaders"
    }

    fn subject(&self) -> HashSet<UaRole> {
        HashSet::from([UaRole::Uas])
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], _bind_key: &str) -> Vec<String> {
        let parser = super::lenient_parser();
        let mut out = Vec::new();
        for msg in sent_messages(events, &parser) {
            let SipMessage::Response(_) = &msg else {
                continue;
            };
            let st = status(&msg);
            if !(100..200).contains(&st) {
                continue;
            }
            let rseq_values = get_headers(msg_headers(&msg), "rseq");
            let require = get_headers(msg_headers(&msg), "require");
            let has_rseq = !rseq_values.is_empty();
            let has_100rel = has_option_tag(&require, "100rel");

            if st == 100 {
                if has_rseq {
                    out.push(
                        "100 (Trying) response carries RSeq — RFC 3262 §3 forbids reliable 100 \
                         (RFC3262-MUST-003 / RFC3262-MUST-007)"
                            .to_string(),
                    );
                }
                if has_100rel {
                    out.push(
                        "100 (Trying) response carries Require: 100rel — RFC 3262 §3 forbids \
                         reliable 100 (RFC3262-MUST-003)"
                            .to_string(),
                    );
                }
                continue;
            }

            // 101-199: reliable 1xx is signalled by Require: 100rel.
            if !has_100rel {
                continue;
            }
            if !has_rseq {
                out.push(format!(
                    "Reliable {st} response carries Require: 100rel but no RSeq header — \
                     RFC 3262 §3 (RFC3262-MUST-007)",
                ));
                continue;
            }
            let raw = rseq_values[0].trim();
            match raw.parse::<u64>() {
                Ok(rseq) if (1..=RSEQ_MAX).contains(&rseq) => {}
                _ => out.push(format!(
                    "Reliable {st} response RSeq={raw} outside [1, 2^31-1] — RFC 3262 §3 \
                     (RFC3262-MUST-008)",
                )),
            }
        }
        out
    }
}

/// The peer rules defined in this module. Aggregated by [`super::rfc_peer_rules`].
pub(crate) fn peer_rules() -> Vec<Arc<dyn PeerAuditRule>> {
    vec![
        Arc::new(No100relRequireOnNonInviteRule),
        Arc::new(Reliable1xxHeadersRule),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    fn sent_at(bind: &str, raw: Vec<u8>, to: &str, seq: u64) -> Stamped<SignalingNetworkEvent> {
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

    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                packet: UdpPacket { raw, src: "127.0.0.1:9999".parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    /// A request with caller-chosen method and an optional extra header row.
    fn req(method: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A 1xx response with caller-chosen status and extra header rows.
    fn resp(status_line: &str, extra: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status_line}\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@127.0.0.1>;tag=at\r\n\
             To: <sip:bob@127.0.0.1>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    // --- no100relRequireOnNonInvite ---

    #[test]
    fn invite_with_100rel_require_is_clean() {
        let evs = vec![sent_at(
            "alice",
            req("INVITE", "Require: 100rel\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        assert!(No100relRequireOnNonInviteRule.check(&evs, "alice").is_empty());
    }

    #[test]
    fn non_invite_with_100rel_require_is_flagged() {
        let evs = vec![sent_at(
            "alice",
            req("BYE", "Require: 100rel\r\n"),
            "127.0.0.1:5070",
            0,
        )];
        let f = No100relRequireOnNonInviteRule.check(&evs, "alice");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-017"), "{}", f[0]);
    }

    #[test]
    fn received_non_invite_100rel_is_not_judged_by_sender_rule() {
        let evs = vec![recv_at("alice", req("BYE", "Require: 100rel\r\n"), 0)];
        assert!(No100relRequireOnNonInviteRule.check(&evs, "alice").is_empty());
    }

    // --- reliable1xxHeaders ---

    #[test]
    fn reliable_183_with_valid_rseq_is_clean() {
        let evs = vec![sent_at(
            "bob",
            resp("183 Session Progress", "Require: 100rel\r\nRSeq: 1\r\n"),
            "127.0.0.1:5060",
            0,
        )];
        assert!(Reliable1xxHeadersRule.check(&evs, "bob").is_empty());
    }

    #[test]
    fn plain_100_trying_is_clean() {
        let evs = vec![sent_at("bob", resp("100 Trying", ""), "127.0.0.1:5060", 0)];
        assert!(Reliable1xxHeadersRule.check(&evs, "bob").is_empty());
    }

    #[test]
    fn reliable_100_with_rseq_is_flagged() {
        let evs = vec![sent_at(
            "bob",
            resp("100 Trying", "RSeq: 1\r\n"),
            "127.0.0.1:5060",
            0,
        )];
        let f = Reliable1xxHeadersRule.check(&evs, "bob");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-003"), "{}", f[0]);
    }

    #[test]
    fn reliable_1xx_missing_rseq_is_flagged() {
        let evs = vec![sent_at(
            "bob",
            resp("183 Session Progress", "Require: 100rel\r\n"),
            "127.0.0.1:5060",
            0,
        )];
        let f = Reliable1xxHeadersRule.check(&evs, "bob");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-007"), "{}", f[0]);
    }

    #[test]
    fn reliable_1xx_out_of_range_rseq_is_flagged() {
        let evs = vec![sent_at(
            "bob",
            resp("183 Session Progress", "Require: 100rel\r\nRSeq: 0\r\n"),
            "127.0.0.1:5060",
            0,
        )];
        let f = Reliable1xxHeadersRule.check(&evs, "bob");
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("MUST-008"), "{}", f[0]);
    }
}
