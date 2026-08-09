//! **RFC 3261 §17.2.1 — a server transaction emits exactly ONE final response.**
//! The cross-message rule over the responses a lane SENDS: once a final (≥ 200)
//! has gone out on a server transaction, the only further response that
//! transaction may emit is a retransmission of that same final (§17.2.1 for a
//! non-2xx, §13.3.1.4 for a 2xx). A second final with a DIFFERENT status is a
//! violation — the caller has already ACKed the first one and moved on, so the
//! second lands on a transaction that no longer exists.
//!
//! Sibling of [`super::rfc3261_cross::No1xxAfterFinalRule`], which owns the
//! after-final **provisional**; this rule never judges a 1xx.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};
use sip_message::{SipMessage, SipParser};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};
use crate::report::to_sip_entries;
use crate::rfc_audit::dialog_model::{
    call_id, cseq_method, cseq_seq, relay_lanes, status, top_via_branch,
};
use crate::types::UaRole;

/// Identity of one server transaction on one lane: the sending lane, the
/// Call-ID, the top-Via branch and the CSeq `(number, method)` the responses
/// answer.
///
/// Branch + method is the RFC 3261 §17 transaction key — method is load-bearing
/// because a CANCEL shares its INVITE's branch (§9.1), so a `200 OK (CANCEL)`
/// and the `487 (INVITE)` that follows it are two transactions on one branch.
/// Call-ID and CSeq number ride along so a fixture that reuses a hard-coded
/// branch across calls or requests cannot fold two transactions into one key:
/// every response of one real server transaction repeats both, and a response
/// bearing the wrong one is `responseCorrelation` /
/// `responseCseqMatchesTransaction`'s finding to make.
type ServerTxnKey = (LaneKey, String, String, u32, String);

/// The finals a lane has sent on one server transaction: the first one (the
/// status every legal retransmission repeats) and the divergent statuses already
/// reported, so a Timer-G retransmit of an offending final is one finding, not
/// one per copy.
struct FinalsSent {
    first: u16,
    reported: HashSet<u16>,
}

/// **RFC 3261 §17.2.1 — one final per server transaction.**
/// Fires on the second final response a lane sends on one server transaction
/// with a status differing from the first. Same-status retransmissions are legal
/// and never fire; provisionals are out of scope (see
/// [`super::rfc3261_cross::No1xxAfterFinalRule`]).
///
/// Subject `{Uas}` plus a relay-lane skip: the rule judges what a lane
/// **authored**. A `{Proxy}`-declared bind, or a lane that forwards both
/// directions of one Call-ID, relays whatever the upstream produced — the
/// divergent pair is the upstream's defect and is flagged on the upstream lane.
/// The skip has recording granularity, not per-call: a lane classified relay in
/// ANY dialog slice is unjudged for the whole recording (the semantics
/// [`relay_lanes`] shares with the sibling rule).
pub struct SingleFinalPerServerTxnRule;

impl CrossMessageAuditRule for SingleFinalPerServerTxnRule {
    fn name(&self) -> &'static str {
        "rfc3261.singleFinalPerServerTxn"
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
        let relays = relay_lanes(events);
        let parser = super::lenient_parser();
        let mut txns: HashMap<ServerTxnKey, FinalsSent> = HashMap::new();
        let mut out = Vec::new();

        for (i, entry) in to_sip_entries(events).into_iter().enumerate() {
            let Some(sender) = entry.from_lane.clone() else { continue };
            if relays.contains(&sender) {
                continue;
            }
            let Ok(msg) = parser.parse(&entry.raw) else { continue };
            if !matches!(msg, SipMessage::Response(_)) {
                continue;
            }
            let code = status(&msg);
            if code < 200 {
                continue; // a provisional after the final is the sibling rule's finding
            }
            let Some(branch) = top_via_branch(&msg) else { continue };
            let method = cseq_method(&msg).to_ascii_uppercase();
            let key = (
                sender.clone(),
                call_id(&msg).to_string(),
                branch.clone(),
                cseq_seq(&msg),
                method.clone(),
            );

            if let Some(sent) = txns.get_mut(&key) {
                let first = sent.first;
                if code != first && sent.reported.insert(code) {
                    out.push((
                        sender,
                        format!(
                            "Sent a second final {code} on the {method} server transaction \
                             (callId {}, branch {branch}) that already answered {first} — a \
                             server transaction emits exactly one final response; only a \
                             same-status retransmission is legal (RFC 3261 §17.2.1 / §13.3.1.4)",
                            call_id(&msg),
                        ),
                        Some(i + 1),
                    ));
                }
            } else {
                txns.insert(key, FinalsSent { first: code, reported: HashSet::new() });
            }
        }
        out
    }
}

/// The cross-message rules defined in this module. Aggregated by
/// [`super::rfc_cross_message_rules`].
pub(crate) fn cross_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![Arc::new(SingleFinalPerServerTxnRule)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rfc_audit::rfc3261_cross::No1xxAfterFinalRule;
    use crate::rfc_audit::{evaluate_rfc_findings, RfcFinding};
    use crate::types::{BindSummary, UdpPacket};

    // `to_sip_entries` resolves lanes by ADDRESS, so the traces use addr bind
    // keys: the SUT answering at :5080, the caller at :5060, the callee at :5070.
    const SUT: &str = "127.0.0.1:5080";
    const ALICE: &str = "127.0.0.1:5060";
    const BOB: &str = "127.0.0.1:5070";
    const A: &str = "sip:alice@127.0.0.1";
    const B: &str = "sip:bob@127.0.0.1";

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

    /// A `BindAcquire` declaring `roles` for `bind` — what subject dispatch in
    /// [`evaluate_rfc_findings`] reads.
    fn bind_roles(bind: &str, roles: HashSet<UaRole>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::BindAcquire {
                bind_key: bind.to_string(),
                summary: BindSummary {
                    addr: bind.parse().unwrap(),
                    queue_max: 256,
                    reuse_port: false,
                    roles,
                    has_pre_ingress: false,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    fn req(method: &str, branch: &str, cseq: u32, ttag: Option<&str>) -> Vec<u8> {
        req_in(method, branch, cseq, ttag, "cid-1@127.0.0.1")
    }

    fn req_in(method: &str, branch: &str, cseq: u32, ttag: Option<&str>, cid: &str) -> Vec<u8> {
        let to = match ttag {
            Some(t) => format!("<{B}>;tag={t}"),
            None => format!("<{B}>"),
        };
        format!(
            "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn resp(status: u16, cseq: u32, method: &str, ttag: &str, branch: &str) -> Vec<u8> {
        resp_in(status, cseq, method, ttag, branch, "cid-1@127.0.0.1")
    }

    fn resp_in(
        status: u16,
        cseq: u32,
        method: &str,
        ttag: &str,
        branch: &str,
        cid: &str,
    ) -> Vec<u8> {
        // Empty `ttag` ⇒ a tagless To (a 100 Trying); `;tag=` with an empty
        // value is not a wire token and even the lenient parser rejects it.
        let to = if ttag.is_empty() { format!("<{B}>") } else { format!("<{B}>;tag={ttag}") };
        format!(
            "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The b-leg INVITE a B2BUA originates, carrying its OWN Call-ID — each leg
    /// is a distinct dialog, which is what keeps a B2BUA lane out of the relay
    /// classification that exempts a forwarding proxy.
    fn b_leg_invite(branch: &str) -> Vec<u8> {
        format!(
            "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn second_different_final_is_flagged() {
        // The 068 / 069(b) signature: the caller CANCELs, the SUT answers the
        // INVITE 487 and the caller ACKs — then a later decision authors a 480
        // on the same server transaction.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 3),
        ];
        let out = SingleFinalPerServerTxnRule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT, "attributed to the lane that authored the second final");
        assert!(out[0].1.contains("480") && out[0].1.contains("487"), "{}", out[0].1);
        // The wire view lists all four messages in order (an arrival from a
        // non-recorded sender is its own entry), so the late 480 is entry 4.
        assert_eq!(out[0].2, Some(4), "offending points at the late 480");
    }

    #[test]
    fn same_status_retransmissions_are_silent() {
        // §17.2.1 Timer G retransmits the non-2xx final, §13.3.1.4 the 2xx —
        // same status, same transaction, legal.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-g", 1, None), ALICE, 0),
            sent(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-g"), ALICE, 1),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 2),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 3),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-g"), ALICE, 4),
            recv(SUT, req("ACK", "z9hG4bK-g", 1, Some("bt")), ALICE, 5),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn later_1xx_stays_with_no_1xx_after_final() {
        // A provisional after the final is the sibling rule's finding, never
        // this one's — the ownership boundary, asserted from both sides.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-p", 1, None), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-p"), ALICE, 1),
            sent(SUT, resp(183, 1, "INVITE", "bt", "z9hG4bK-p"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-p", 1, Some("bt")), ALICE, 3),
        ];
        assert!(
            SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty(),
            "a 1xx is not a final — this rule does not judge it"
        );
        assert_eq!(
            No1xxAfterFinalRule.check(&evs).len(),
            1,
            "the sibling rule owns the after-final provisional"
        );
    }

    #[test]
    fn cancel_200_and_invite_487_share_a_branch_cleanly() {
        // A CANCEL reuses its INVITE's branch (§9.1): the `200 OK (CANCEL)` and
        // the `487 (INVITE)` are two server transactions, not two finals on one.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-c", 1, None), ALICE, 0),
            sent(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-c"), ALICE, 1),
            recv(SUT, req("CANCEL", "z9hG4bK-c", 1, None), ALICE, 2),
            sent(SUT, resp(200, 1, "CANCEL", "bt", "z9hG4bK-c"), ALICE, 3),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-c"), ALICE, 4),
            recv(SUT, req("ACK", "z9hG4bK-c", 1, Some("bt")), ALICE, 5),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn relay_lane_forwarding_two_finals_is_not_judged() {
        // A lane that forwards both directions of one Call-ID relays what the
        // upstream produced; the divergent pair is flagged on the upstream lane.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn b2bua_lane_authoring_its_a_leg_finals_is_judged() {
        // The same shape as the relay case, except the forwarded INVITE carries
        // the B2BUA's own b-leg Call-ID: the a-leg finals are the B2BUA's own
        // emissions, so the relay exemption must not swallow them.
        let evs = vec![
            recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
            sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 1),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
            recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
        ];
        let out = SingleFinalPerServerTxnRule.check(&evs);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, SUT);
    }

    #[test]
    fn each_divergent_status_is_reported_exactly_once() {
        // The offending final retransmitted (Timer G) is ONE finding, not one
        // per copy; a genuinely third status is still its own finding.
        let retransmitted = vec![
            recv(SUT, req("INVITE", "z9hG4bK-r", 1, None), ALICE, 0),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 1),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 2),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 3),
            recv(SUT, req("ACK", "z9hG4bK-r", 1, Some("bt")), ALICE, 4),
        ];
        let out = SingleFinalPerServerTxnRule.check_positioned(&retransmitted);
        assert_eq!(out.len(), 1, "one finding per divergent status, not per copy: {out:?}");
        assert_eq!(out[0].2, Some(3), "attributed to the FIRST copy of the offending final");

        let three = vec![
            recv(SUT, req("INVITE", "z9hG4bK-r", 1, None), ALICE, 0),
            sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 1),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 2),
            sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 3),
            sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 4),
            recv(SUT, req("ACK", "z9hG4bK-r", 1, Some("bt")), ALICE, 5),
        ];
        let out = SingleFinalPerServerTxnRule.check_positioned(&three);
        assert_eq!(out.len(), 2, "a third distinct status is its own finding: {out:?}");
        assert!(out[0].1.contains("480") && out[1].1.contains("486"), "{out:?}");
    }

    #[test]
    fn non_invite_server_txn_is_judged() {
        // §17.2.2: the one-final rule is not INVITE-specific — a BYE server
        // transaction answered twice with different statuses is a violation,
        // while the retransmitted 200 is legal.
        let diverging = vec![
            recv(SUT, req("BYE", "z9hG4bK-n", 2, Some("bt")), ALICE, 0),
            sent(SUT, resp(200, 2, "BYE", "bt", "z9hG4bK-n"), ALICE, 1),
            sent(SUT, resp(481, 2, "BYE", "bt", "z9hG4bK-n"), ALICE, 2),
        ];
        assert_eq!(SingleFinalPerServerTxnRule.check(&diverging).len(), 1);

        let retransmitted = vec![
            recv(SUT, req("BYE", "z9hG4bK-n", 2, Some("bt")), ALICE, 0),
            sent(SUT, resp(200, 2, "BYE", "bt", "z9hG4bK-n"), ALICE, 1),
            sent(SUT, resp(200, 2, "BYE", "bt", "z9hG4bK-n"), ALICE, 2),
        ];
        assert!(SingleFinalPerServerTxnRule.check(&retransmitted).is_empty());
    }

    #[test]
    fn two_calls_reusing_one_branch_are_distinct_transactions() {
        // Raw-injection fixtures share hard-coded branches and `CSeq 1 INVITE`;
        // the Call-ID in the key keeps two compliant calls from folding into one
        // transaction and false-firing this gating rule.
        let evs = vec![
            recv(SUT, req_in("INVITE", "z9hG4bK-1", 1, None, "call-a@h"), ALICE, 0),
            sent(SUT, resp_in(200, 1, "INVITE", "bt", "z9hG4bK-1", "call-a@h"), ALICE, 1),
            recv(SUT, req_in("ACK", "z9hG4bK-1", 1, Some("bt"), "call-a@h"), ALICE, 2),
            recv(SUT, req_in("INVITE", "z9hG4bK-1", 1, None, "call-b@h"), ALICE, 3),
            sent(SUT, resp_in(486, 1, "INVITE", "bt2", "z9hG4bK-1", "call-b@h"), ALICE, 4),
            recv(SUT, req_in("ACK", "z9hG4bK-1", 1, Some("bt2"), "call-b@h"), ALICE, 5),
        ];
        assert!(SingleFinalPerServerTxnRule.check_positioned(&evs).is_empty());
    }

    #[test]
    fn proxy_declared_lane_is_out_of_subject() {
        // Subject dispatch: the same trace fires for a `{Uac, Uas}` bind and is
        // silent for a `{Proxy}`-declared one.
        let wire = |bind: &str| {
            vec![
                recv(bind, req("INVITE", "z9hG4bK-s", 1, None), ALICE, 1),
                sent(bind, resp(487, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 2),
                recv(bind, req("ACK", "z9hG4bK-s", 1, Some("bt")), ALICE, 3),
                sent(bind, resp(603, 1, "INVITE", "bt", "z9hG4bK-s"), ALICE, 4),
            ]
        };
        let fired = |evs: &[Stamped<SignalingNetworkEvent>]| -> Vec<RfcFinding> {
            evaluate_rfc_findings(evs)
                .into_iter()
                .filter(|f| f.rule == "rfc3261.singleFinalPerServerTxn")
                .collect()
        };

        let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
        ua.extend(wire(SUT));
        let found = fired(&ua);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].advisory, "the rule gates");

        let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
        proxy.extend(wire(SUT));
        assert!(fired(&proxy).is_empty(), "a {{Proxy}} bind is not the subject");
    }
}
