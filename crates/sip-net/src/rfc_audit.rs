//! **TEST-ONLY.** RFC 3261 audit rules over the recorded signaling layer.
//!
//! These run at layer close (when wired into [`ScopedAuditOptions`](crate::ScopedAuditOptions))
//! or directly over a channel snapshot. They flag on-wire protocol invariants
//! that a real UAC/UAS enforces but the test UAs (which answer whatever they are
//! handed, regardless of CSeq) do not — so the recording itself, not the per-step
//! `expect`, becomes the place those invariants are checked. Wiring them into the
//! default options gives every harness the same "post-run all-clean" CSeq check
//! that the live SIPp endpoints apply in endurance.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::{extract_tag, get_header, get_headers, parse_via_params};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::contracts::{CrossMessageAuditRule, SignalingNetworkEvent};

/// Per-request-stream state for the in-dialog CSeq audit. A *stream* is one
/// `(receiving endpoint, Call-ID, From-tag)` — one originating UA's request flow
/// in one direction. Within a stream each **dialog** is tracked separately by its
/// To-tag (`""` for the dialog-creating request that has no To-tag yet): forking
/// materialises several dialogs that share a Call-ID + From-tag but diverge by
/// To-tag, and each owns an independent CSeq space.
#[derive(Default)]
struct StreamState {
    /// Transactions already observed, keyed by `(top-Via branch, method, CSeq)`,
    /// to fold away on-wire retransmissions. A genuine retransmission repeats all
    /// three; method + CSeq are part of the key (not the branch alone) because the
    /// simulated fabric's per-worker `IdGen` resets on a failover restart, so a
    /// *different* request relayed by the backup can reuse a branch the crashed
    /// primary spent — keying on the branch alone would mis-skip it as a phantom
    /// retransmit (RFC 3261 assumes globally-unique branches, §8.1.1.7).
    seen_txns: HashSet<(String, String, u32)>,
    /// Per dialog (To-tag, `""` = dialog-creating) → CSeq of the last *new*
    /// (non-ACK/CANCEL) request seen on that dialog.
    last_new_cseq: HashMap<String, u32>,
}

/// **RFC 3261 §12.2.1.1 — in-dialog request sequencing.** Within a dialog the
/// UAC MUST increment the CSeq sequence number by **exactly one** for each new
/// request (ACK and CANCEL excepted — they reuse the CSeq of the request they
/// acknowledge/cancel). A UAS rejects a new in-dialog request whose CSeq is not
/// exactly one greater than the dialog's last: lower/equal → 500 out of order or
/// silently dropped as a retransmission; a gap (≥ +2) violates the
/// increment-by-one rule. An on-wire retransmission reuses both its CSeq *and*
/// its top-Via branch — exempt.
///
/// **Per-dialog, not per-leg.** The check keys each stream by `(receiving
/// endpoint, Call-ID, From-tag)` and then tracks each dialog **by To-tag**. This
/// matters under forking (RFC 3261 §12.1.2 / §13.2.2.4): one INVITE creates
/// several early dialogs that share Call-ID + From-tag but each carry a distinct
/// callee To-tag, and each maintains its OWN CSeq sequence seeded from the
/// INVITE. So two forks' first PRACKs both at `INVITE_CSeq + 1` is **correct**
/// (distinct dialogs), not a reuse — conflating them by ignoring the To-tag would
/// be a false positive. The dialog-creating INVITE (no To-tag, tracked under
/// `""`) is the baseline: a forked/confirmed dialog's first in-dialog request
/// must be `INVITE_CSeq + 1`, and that same To-tag's sequence then advances by
/// one per request as the early dialog becomes the confirmed one.
///
/// Within a stream it distinguishes a *retransmission* from a *new transaction*
/// by the top (first) `Via` header's `branch=` token (a retransmission reuses
/// it). This is the teeth for (a) a takeover that probes a dialog with a stale
/// (pre-failover) CSeq snapshot — the survivor's OPTIONS lands at/below the CSeq
/// the callee already saw — and (b) a keepalive loop that never increments the
/// dialog CSeq, so each new OPTIONS (and the eventual BYE) reuses the previous
/// request's CSeq. Both are invisible to a test UA that answers whatever it is
/// handed.
pub struct CSeqInDialogOrderRule;

/// The `branch=` token of the TOP (first) `Via` header, if present and
/// non-empty. A retransmission reuses this exact token; a new client
/// transaction mints a fresh one.
fn top_via_branch(req: &sip_message::SipRequest) -> Option<String> {
    let top = get_headers(&req.headers, "via").into_iter().next()?;
    parse_via_params(top).branch.filter(|b| !b.is_empty())
}

impl CrossMessageAuditRule for CSeqInDialogOrderRule {
    fn name(&self) -> &'static str {
        "rfc3261.cseqInDialogOrder"
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        // (receiving bind, Call-ID, From-tag) -> stream state.
        let mut streams: HashMap<(LaneKey, String, String), StreamState> = HashMap::new();
        let mut findings = Vec::new();
        let parser = CustomParser::new();
        for s in events {
            let SignalingNetworkEvent::RecvItem { bind_key, packet } = &s.event else {
                continue;
            };
            let Ok(SipMessage::Request(req)) = parser.parse(&packet.raw) else {
                continue; // responses / unparseable: not our concern here
            };
            let (Some(call_id), Some(from_tag)) = (
                get_header(&req.headers, "call-id").map(str::to_string),
                get_header(&req.headers, "from").and_then(extract_tag),
            ) else {
                continue;
            };
            let key = (bind_key.clone(), call_id, from_tag);
            let st = streams.entry(key.clone()).or_default();
            let seq = req.cseq.seq;
            let method = req.method.to_string();

            // 1. A repeat of the SAME (branch, method, CSeq) is a retransmission
            //    of a transaction we already accounted for: skip entirely (no flag,
            //    no state). Method + CSeq guard against an `IdGen`-reset branch
            //    collision masquerading as a retransmit (see `seen_txns`).
            if let Some(branch) = top_via_branch(&req) {
                let txn = (branch, method.clone(), seq);
                if st.seen_txns.contains(&txn) {
                    continue;
                }
                // 2. Record this transaction.
                st.seen_txns.insert(txn);
            }

            // 3. ACK/CANCEL legitimately reuse the related request's CSeq:
            //    exempt, and they do not advance the per-dialog sequence.
            if method.eq_ignore_ascii_case("ACK") || method.eq_ignore_ascii_case("CANCEL") {
                continue;
            }

            // 4. A genuinely new in-dialog request. The dialog it belongs to is
            //    identified by the To-tag (`""` for the dialog-creating request
            //    that has none yet); each dialog owns an independent CSeq space.
            let to_tag = req.to.tag.clone().unwrap_or_default();

            // `prev` is the number this request must be exactly one greater than:
            //   - subsequent request on this dialog → its last new CSeq;
            //   - first in-dialog request of a (forked/confirmed) dialog → the
            //     dialog-creating INVITE's CSeq (the `""` baseline), if observed.
            // A dialog-creating request (empty To-tag) only seeds the baseline.
            let prev = match st.last_new_cseq.get(&to_tag) {
                Some(&last) => Some(last),
                None if to_tag.is_empty() => None,
                None => st.last_new_cseq.get("").copied(),
            };

            if let Some(prev) = prev {
                if seq != prev + 1 {
                    findings.push((
                        bind_key.clone(),
                        cseq_violation_msg(&method, seq, prev, &key.1, &key.2, &to_tag),
                    ));
                }
            }
            st.last_new_cseq.insert(to_tag, seq);
        }
        findings
    }
}

/// Phrase the §12.2.1.1 violation for `{method} CSeq {seq}` when the dialog's
/// last new CSeq (or, for a dialog's first in-dialog request, the dialog-creating
/// INVITE's CSeq) was `prev` and the only RFC-legal value is `prev + 1`.
fn cseq_violation_msg(
    method: &str,
    seq: u32,
    prev: u32,
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
) -> String {
    let dialog = format!(
        "Call-ID={call_id} from-tag={from_tag} to-tag={}",
        if to_tag.is_empty() { "<none>" } else { to_tag },
    );
    if seq == prev {
        format!(
            "in-dialog CSeq reused (RFC 3261 §12.2.1.1): {method} CSeq {seq} reuses a prior \
             request's CSeq (a new in-dialog transaction must increment the dialog CSeq by \
             exactly one) on {dialog} — a real UAS treats this as a retransmission and drops the \
             new request (the test UA answers it, hiding the bug)"
        )
    } else if seq < prev {
        format!(
            "in-dialog CSeq regressed (RFC 3261 §12.2.1.1): {method} CSeq {seq} arrived after \
             CSeq {prev} (out of order) on {dialog} — a real UAS rejects this 500 (the test UA \
             answers it, hiding the bug)"
        )
    } else {
        format!(
            "in-dialog CSeq not contiguous (RFC 3261 §12.2.1.1): {method} CSeq {seq} skips ahead \
             of CSeq {prev}; the UAC MUST increment the dialog CSeq by exactly one (expected \
             {}) on {dialog}",
            prev + 1,
        )
    }
}

/// The built-in RFC 3261 cross-message audit rules every test harness installs by
/// default (see [`crate::with_all_contracts`]). Currently the in-dialog CSeq
/// ordering check; add further wire invariants here.
pub fn rfc_cross_message_rules() -> Vec<Arc<dyn CrossMessageAuditRule>> {
    vec![Arc::new(CSeqInDialogOrderRule)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    /// A minimal, parseable in-dialog request. `method`, `cseq` and the
    /// top-Via `branch` are all caller-controlled so a test can model a new
    /// transaction (fresh branch), a retransmission (reused branch), or an
    /// ACK/CANCEL that reuses a CSeq.
    fn req(method: &str, call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag={from_tag}\r\n\
             To: <sip:bob@127.0.0.1>;tag=btag\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A new-transaction OPTIONS (distinct branch derived from the CSeq).
    fn options(call_id: &str, from_tag: &str, cseq: u32) -> Vec<u8> {
        req("OPTIONS", call_id, from_tag, cseq, &format!("z9hG4bK-{cseq}"))
    }

    /// Like [`req`] but with an explicit dialog To-tag — `None` models the
    /// dialog-creating request (initial INVITE) that has no To-tag yet, and a
    /// distinct `Some(tag)` per fork models the forked early dialogs that share a
    /// Call-ID + From-tag but diverge by To-tag.
    fn req_to(
        method: &str,
        call_id: &str,
        from_tag: &str,
        cseq: u32,
        branch: &str,
        to_tag: Option<&str>,
    ) -> Vec<u8> {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@127.0.0.1>;tag={t}"),
            None => "<sip:bob@127.0.0.1>".to_string(),
        };
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag={from_tag}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// Wrap raw bytes as a `RecvItem` at `bind` (the receiving endpoint).
    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                packet: UdpPacket { raw, src: "127.0.0.1:5091".parse().unwrap(), arrival_ms: seq },
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn in_order_cseq_is_clean() {
        // INVITE(1) / OPTIONS(2) / BYE(3) on distinct branches: strictly
        // increasing across genuinely new transactions — clean.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-o"), 1),
            recv_at("bob", req("BYE", "cid-1", "ft", 3, "z9hG4bK-b"), 2),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn equal_cseq_retransmit_is_clean() {
        // A retransmit reuses its CSeq *and* its top-Via branch — one
        // transaction, equal is allowed, not a reuse-by-a-new-transaction.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-same"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-same"), 1),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn equal_cseq_reuse_different_method_is_flagged() {
        // The exact production bug: OPTIONS keepalive at CSeq 2, then a BYE that
        // *reuses* CSeq 2 on a NEW branch — a new transaction that failed to
        // increment the dialog CSeq.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-x"), 0),
            recv_at("bob", req("BYE", "cid-1", "ft", 2, "z9hG4bK-y"), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the CSeq reuse must be flagged");
        assert_eq!(findings[0].0, "bob", "attributed to the receiving endpoint");
        assert!(findings[0].1.contains("reuses a prior request's CSeq"), "{}", findings[0].1);
    }

    #[test]
    fn equal_cseq_reuse_same_method_new_transaction_is_flagged() {
        // The repeated-keepalive case: two successive OPTIONS at CSeq 2 on
        // DIFFERENT branches — two transactions, the second failed to increment.
        let evs = vec![
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-x"), 0),
            recv_at("bob", req("OPTIONS", "cid-1", "ft", 2, "z9hG4bK-y"), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the new-transaction CSeq reuse must be flagged");
        assert!(findings[0].1.contains("reuses a prior request's CSeq"), "{}", findings[0].1);
    }

    #[test]
    fn regressed_cseq_is_flagged() {
        // The stale-snapshot takeover shape: CSeq 2 after CSeq 3 on the same
        // (bind, Call-ID, From-tag) stream — out of order, must be flagged.
        let evs = vec![
            recv_at("bob", options("cid-1", "ft", 3), 0),
            recv_at("bob", options("cid-1", "ft", 2), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the regression must be flagged");
        assert_eq!(findings[0].0, "bob", "attributed to the receiving endpoint");
        assert!(findings[0].1.contains("out of order"), "{}", findings[0].1);
    }

    #[test]
    fn ack_reusing_invite_cseq_is_exempt() {
        // ACK for a 2xx INVITE has a NEW branch but reuses the INVITE CSeq:
        // method exemption keeps it clean. Same for a re-INVITE + its ACK.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1"), 0),
            recv_at("bob", req("ACK", "cid-1", "ft", 1, "z9hG4bK-a1"), 1),
            recv_at("bob", req("INVITE", "cid-1", "ft", 2, "z9hG4bK-i2"), 2),
            recv_at("bob", req("ACK", "cid-1", "ft", 2, "z9hG4bK-a2"), 3),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn cancel_reusing_invite_cseq_is_exempt() {
        // CANCEL reuses the CSeq of the INVITE it cancels — exempt.
        let evs = vec![
            recv_at("bob", req("INVITE", "cid-1", "ft", 1, "z9hG4bK-i1"), 0),
            recv_at("bob", req("CANCEL", "cid-1", "ft", 1, "z9hG4bK-c1"), 1),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn separate_streams_do_not_alias() {
        // Different Call-ID, and different From-tag (the two directions of a
        // dialog), each carry an independent CSeq space — no cross-stream alias.
        let evs = vec![
            recv_at("bob", options("cid-A", "ft1", 5), 0),
            recv_at("bob", options("cid-B", "ft1", 1), 1), // other dialog
            recv_at("bob", options("cid-A", "ft2", 1), 2), // other direction
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn forked_early_dialogs_reusing_cseq_across_dialogs_is_clean() {
        // Forking (RFC 3261 §12.1.2): ONE INVITE (no To-tag) creates TWO early
        // dialogs that share Call-ID + From-tag but carry distinct callee To-tags.
        // Each owns its own CSeq space seeded from the INVITE, so BOTH first
        // PRACKs land at INVITE_CSeq + 1 = 2. That is NOT a reuse — distinct
        // dialogs — and the per-dialog (To-tag-keyed) check must stay clean. The
        // confirmed dialog (fork1) then BYEs at the next number IN THAT dialog (3).
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p1", Some("fork1")), 1),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p2", Some("fork2")), 2),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 3, "z9hG4bK-b", Some("fork1")), 3),
        ];
        assert!(
            CSeqInDialogOrderRule.check(&evs).is_empty(),
            "two forks' PRACKs at the same CSeq are distinct dialogs, not a reuse",
        );
    }

    #[test]
    fn forked_dialog_first_request_not_invite_plus_one_is_flagged() {
        // A forked early dialog's FIRST in-dialog request must be exactly the
        // dialog-creating INVITE's CSeq + 1 (= 2). A PRACK at CSeq 3 skips a
        // number — the increment-by-exactly-one rule (§12.2.1.1) is violated.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 3, "z9hG4bK-p", Some("fork1")), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "the +2 jump off the INVITE baseline must be flagged");
        assert!(findings[0].1.contains("not contiguous"), "{}", findings[0].1);
    }

    #[test]
    fn confirmed_dialog_continues_its_own_sequence_by_one() {
        // The early dialog (fork1, PRACK 2) becomes the confirmed dialog and keeps
        // advancing its OWN To-tag's sequence by one: re-INVITE 3 (+ACK 3 exempt),
        // BYE 4. Strictly +1 within the dialog → clean.
        let evs = vec![
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 1, "z9hG4bK-i", None), 0),
            recv_at("bob", req_to("PRACK", "cid-1", "ft", 2, "z9hG4bK-p", Some("fork1")), 1),
            recv_at("bob", req_to("INVITE", "cid-1", "ft", 3, "z9hG4bK-ri", Some("fork1")), 2),
            recv_at("bob", req_to("ACK", "cid-1", "ft", 3, "z9hG4bK-a", Some("fork1")), 3),
            recv_at("bob", req_to("BYE", "cid-1", "ft", 4, "z9hG4bK-b", Some("fork1")), 4),
        ];
        assert!(CSeqInDialogOrderRule.check(&evs).is_empty());
    }

    #[test]
    fn cseq_gap_within_a_dialog_is_flagged() {
        // A +2 jump between two new requests on the SAME dialog violates the
        // increment-by-exactly-one rule even though it is strictly increasing.
        let evs = vec![
            recv_at("bob", options("cid-1", "ft", 2), 0),
            recv_at("bob", options("cid-1", "ft", 4), 1),
        ];
        let findings = CSeqInDialogOrderRule.check(&evs);
        assert_eq!(findings.len(), 1, "a CSeq gap (+2) must be flagged");
        assert!(findings[0].1.contains("not contiguous"), "{}", findings[0].1);
    }
}

