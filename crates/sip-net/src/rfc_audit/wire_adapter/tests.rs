//! The LIVE policy this adapter adds to the merged bodies: subject
//! dispatch, the relay-lane skip, and the wire position a finding anchors
//! on. The rule semantics themselves are pinned in `rfc_rules::rules`.

use std::collections::HashSet;

use super::*;
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
            wire: crate::contracts::WireStamp::of_bytes(&raw),
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
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, cseq: u32, method: &str, ttag: &str, branch: &str) -> Vec<u8> {
    // Empty `ttag` ⇒ a tagless To (a 100 Trying); `;tag=` with an empty
    // value is not a wire token and even the lenient parser rejects it.
    let to = if ttag.is_empty() { format!("<{B}>") } else { format!("<{B}>;tag={ttag}") };
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
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

/// A request the SUT sends on its own b-leg — the leg it drives as UAC,
/// which is where a CANCEL of its own making rides.
fn b_leg_req(method: &str, branch: &str, ttag: Option<&str>) -> Vec<u8> {
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: {to}\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The far end's answer on that b-leg.
fn b_leg_resp(status: u16, branch: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// RFC 3261 §9.1 at the CANCEL's sender: the SUT's own ACK is what proves
/// the transaction was over. A CANCEL AHEAD of that ACK may have crossed
/// the final in flight and is not charged; one behind it cannot have, and
/// is.
#[test]
fn a_cancel_the_sut_sent_after_acking_its_own_final_is_flagged() {
    let crossed = vec![
        sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
        recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
        sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 2),
        sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 3),
    ];
    assert!(
        NoCancelAfterFinalRule.check_positioned(&crossed).is_empty(),
        "a CANCEL the ACK has not yet sealed may have crossed the final"
    );

    let late = vec![
        sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
        recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
        sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 2),
        sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 3),
    ];
    let out = NoCancelAfterFinalRule.check_positioned(&late);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that sent the CANCEL is charged");
    assert_eq!(out[0].3.as_deref(), Some(SUT));
    assert!(out[0].1.contains("486") && out[0].1.contains("§9.1"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the late CANCEL");
    assert!(!NoCancelAfterFinalRule.force_advisory(), "the late CANCEL gates");
}

/// The far end's own late CANCEL is not this bind's: the rule charges the
/// UAC that sent it, and the live policy surfaces a finding only where the
/// charged party IS the vantage.
#[test]
fn a_late_cancel_the_peer_sent_is_not_charged_to_this_bind() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
        recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 3),
    ];
    assert!(NoCancelAfterFinalRule.check_positioned(&evs).is_empty());
}

#[test]
fn second_different_final_is_flagged_at_its_wire_position() {
    // The caller CANCELs, the SUT answers the INVITE 487 and the caller
    // ACKs — then a later decision authors a 480 on that transaction.
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
    assert!(out[0].1.contains("z9hG4bK-i") && out[0].1.contains("INVITE"), "{}", out[0].1);
    // The wire view lists all four messages in order (an arrival from a
    // non-recorded sender is its own entry), so the late 480 is entry 4.
    assert_eq!(out[0].2, Some(4), "offending points at the late 480");
}

#[test]
fn every_divergent_status_is_named_on_the_one_finding() {
    // One occasion per transaction: the detail names 480 AND the later 486,
    // so no status goes unreported.
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-r", 1, None), ALICE, 0),
        sent(SUT, resp(487, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 1),
        sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 2),
        sent(SUT, resp(480, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 3),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-r"), ALICE, 4),
        recv(SUT, req("ACK", "z9hG4bK-r", 1, Some("bt")), ALICE, 5),
    ];
    let out = SingleFinalPerServerTxnRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "one finding for the transaction: {out:?}");
    assert!(
        out[0].1.contains("480") && out[0].1.contains("486") && out[0].1.contains("487"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(3), "attributed to the FIRST copy of the offending final");
}

#[test]
fn same_status_retransmissions_are_silent() {
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

/// A request carrying an explicit header block — the §9.1 Route rules read
/// the bytes, so a builder that can vary them is what those tests need.
fn req_with(method: &str, branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[test]
fn divergent_cancel_route_is_flagged_at_its_wire_position() {
    let evs = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
        sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p2@h;lr>\r\n"), BOB, 1),
    ];
    let out = CancelRouteEchoesInviteRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that sent the CANCEL");
    assert!(out[0].1.contains("differ") && out[0].1.contains("z9hG4bK-r"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the CANCEL");

    let echoed = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
        sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
    ];
    assert!(CancelRouteEchoesInviteRule.check_positioned(&echoed).is_empty());
}

#[test]
fn relay_lane_passing_a_cancel_through_is_not_judged() {
    // A lane that forwards both directions of one Call-ID passes the
    // upstream UAC's CANCEL through; a divergence is the upstream's.
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req_with("INVITE", "z9hG4bK-b", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
        sent(SUT, req_with("CANCEL", "z9hG4bK-b", "Route: <sip:p2@h;lr>\r\n"), BOB, 2),
    ];
    assert!(CancelRouteEchoesInviteRule.check_positioned(&evs).is_empty());
}

#[test]
fn eager_cancel_is_advisory_and_noted_on_a_forwarding_lane() {
    // The rule never gates (ADR-0028), and a B2BUA/AS lane authoring its
    // own b-leg CANCEL is judged on it — no relay exemption here.
    assert!(CancelAfter1xxRule.force_advisory());
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
    ];
    let out = CancelAfter1xxRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("before any received"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the CANCEL");

    // A taken provisional discharges it.
    let waited = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        recv(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 2),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 3),
    ];
    assert!(CancelAfter1xxRule.check_positioned(&waited).is_empty());
}

#[test]
fn proxy_declared_lane_sends_no_eager_cancel_finding() {
    // A `{Proxy}`-only lane forwards the upstream UAC's CANCEL at the
    // upstream's timing: exempt, where a `{Uac, Uas}` lane is judged.
    let wire = vec![
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
    ];
    let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
    ua.extend(wire.clone());
    assert_eq!(CancelAfter1xxRule.check(&ua).len(), 1, "a UA lane is judged");

    let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
    proxy.extend(wire);
    assert!(CancelAfter1xxRule.check(&proxy).is_empty(), "a {{Proxy}} lane is exempt");
}

/// The CSeq family is surfaced at the TAKER: the lane that RECEIVED the
/// stream reports it, the finding names the far UAC as the offender, and
/// the wire position pins the reusing request — which is what keeps a
/// party-scoped waiver resolving to the sender, not to the reporting lane.
#[test]
fn a_cseq_reuse_is_reported_on_the_lane_that_took_it() {
    let evs = vec![
        recv(SUT, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 0),
        recv(SUT, req("BYE", "z9hG4bK-b", 2, Some("bt")), ALICE, 1),
    ];
    let out = CseqInDialogOrderRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took the stream");
    assert!(out[0].1.contains("reuses a prior request's CSeq"), "{}", out[0].1);
    assert!(out[0].1.contains("to-tag=bt"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the reusing BYE");
}

/// A fully recorded fabric carries every datagram twice — the sender's
/// `SendCalled` and the taker's `RecvItem`. The taker vantage reports it
/// ONCE: the sender's own view judges the same stream but is not its taker.
#[test]
fn a_recorded_sender_does_not_double_report_the_taker_finding() {
    let evs = vec![
        sent(ALICE, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), SUT, 0),
        recv(SUT, req("OPTIONS", "z9hG4bK-o", 2, Some("bt")), ALICE, 1),
        sent(ALICE, req("BYE", "z9hG4bK-b", 2, Some("bt")), SUT, 2),
        recv(SUT, req("BYE", "z9hG4bK-b", 2, Some("bt")), ALICE, 3),
    ];
    let out = CseqInDialogOrderRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "one report, at the taker: {out:?}");
    assert_eq!(out[0].0, SUT);
}

/// §8.1.3.5 at the taker: the SUT opened the transaction, so its OWN send
/// is what the response is judged against — a `200 (INVITE)` carrying a
/// number no request on that branch used is charged to the callee and
/// reported on the lane that took it.
#[test]
fn a_response_cseq_off_its_transaction_is_reported_at_the_taker() {
    let evs = vec![
        sent(SUT, req("INVITE", "z9hG4bK-i", 1, None), BOB, 0),
        recv(SUT, resp(200, 2, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
    ];
    let out = ResponseCseqMatchesTransactionRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("does not match its transaction"), "{}", out[0].1);
    assert!(out[0].1.contains("z9hG4bK-i"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the response");

    // The response the transaction did open for settles it.
    let matched = vec![
        sent(SUT, req("INVITE", "z9hG4bK-i", 1, None), BOB, 0),
        recv(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
    ];
    assert!(ResponseCseqMatchesTransactionRule.check_positioned(&matched).is_empty());
}

/// §13.2.2.4 at the taker: the UAS that cannot match the ACK to its INVITE
/// server transaction is the lane that reports it.
#[test]
fn an_ack_on_an_unknown_cseq_is_reported_at_the_taker() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("ACK", "z9hG4bK-a", 3, Some("bt")), ALICE, 1),
    ];
    let out = AckCseqMatchesInviteRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("acknowledges no INVITE"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the ACK");

    // The ACK that reuses its INVITE's CSeq is silent.
    let matched = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 1),
    ];
    assert!(AckCseqMatchesInviteRule.check_positioned(&matched).is_empty());
}

/// An in-dialog request the SUT sent, with caller-chosen Request-URI,
/// From/To URIs and extra header rows — the §12 rules read those off the
/// bytes, so a builder that can vary them is what those tests need.
#[allow(clippy::too_many_arguments)]
fn in_dialog(
    method: &str,
    r_uri: &str,
    branch: &str,
    cseq: u32,
    from_uri: &str,
    to_uri: &str,
    extra: &str,
) -> Vec<u8> {
    format!(
        "{method} {r_uri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{from_uri}>;tag=at\r\n\
             To: <{to_uri}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A response carrying extra header rows (Record-Route, Allow, …).
fn resp_with(status: u16, cseq: u32, method: &str, branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The establishing exchange every §12 test opens with: the SUT's INVITE
/// and the 200 that confirms the dialog, with `extra` on the 200.
fn established(extra: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", extra), BOB, 1),
    ]
}

/// The dialog-creating INVITE (no To tag) the SUT sends.
fn in_dialog_less_to_tag(branch: &str) -> Vec<u8> {
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// §12.2.1.1 at the EMITTER: the lane that rewrote the dialog's From URI
/// mid-dialog is the lane charged, and the position pins the BYE.
#[test]
fn a_rewritten_mid_dialog_uri_is_reported_on_the_lane_that_sent_it() {
    let mut evs = established("");
    evs.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, "sip:eve@127.0.0.1", B, ""), BOB, 2));
    let out = MidDialogUriRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that sent the request");
    assert!(
        out[0].1.contains("From URI") && out[0].1.contains("sip:eve@127.0.0.1"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(3), "offending points at the BYE");

    let mut clean = established("");
    clean.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
    assert!(MidDialogUriRule.check_positioned(&clean).is_empty());
}

/// A relay carries the originator's From/To through unchanged: per-UA URI
/// stability is not its invariant, so a forwarding lane is not judged.
#[test]
fn a_relay_lane_carrying_a_foreign_uri_is_not_judged() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-b"), BOB, 1),
        recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-b", ""), BOB, 2),
        sent(SUT, in_dialog("BYE", B, "z9hG4bK-y", 2, "sip:eve@127.0.0.1", B, ""), BOB, 3),
    ];
    assert!(MidDialogUriRule.check_positioned(&evs).is_empty());
}

/// §12.2.1.1 route reproduction at the emitter: the 200's Record-Route
/// stack reversed is what the BYE owes.
#[test]
fn a_dropped_mid_dialog_route_set_is_reported_on_the_lane_that_sent_it() {
    let mut evs = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
    evs.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
    let out = MidDialogRouteRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("omits Route header"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the BYE");

    let mut replayed = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
    replayed.push(sent(
        SUT,
        in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, "Route: <sip:p1@127.0.0.1;lr>\r\n"),
        BOB,
        2,
    ));
    assert!(MidDialogRouteRule.check_positioned(&replayed).is_empty());
}

/// §8.1.2 + RFC 3263 §4 at the emitter: the bytes went past the
/// destination the request's own Request-URI names.
#[test]
fn bytes_sent_past_the_derived_destination_are_reported_at_the_emitter() {
    let mut evs = established("");
    evs.push(sent(
        SUT,
        in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""),
        "127.0.0.1:9999",
        2,
    ));
    let out = MidDialogWireDestinationRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("wire-sent to 127.0.0.1:9999"), "{}", out[0].1);

    let mut clean = established("");
    clean.push(sent(
        SUT,
        in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""),
        BOB,
        2,
    ));
    assert!(MidDialogWireDestinationRule.check_positioned(&clean).is_empty());
}

/// §12.1.1 at the TAKER: the lane that TOOK the 100 reports it, and the far
/// endpoint that leaked the Record-Route is the one the text describes.
#[test]
fn record_route_on_a_100_is_reported_at_the_taker() {
    let evs = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 1),
    ];
    assert!(RecordRoutePlacementRule.check_positioned(&evs).is_empty(), "a bare 100 is clean");

    let leaked = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(
            SUT,
            format!(
                "SIP/2.0 100 Trying\r\n\
                     Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i\r\n\
                     From: <{A}>;tag=at\r\n\
                     To: <{B}>\r\n\
                     Call-ID: cid-1@127.0.0.1\r\n\
                     CSeq: 1 INVITE\r\n\
                     Record-Route: <sip:p1@127.0.0.1;lr>\r\n\
                     Content-Length: 0\r\n\r\n"
            )
            .into_bytes(),
            BOB,
            1,
        ),
    ];
    let out = RecordRoutePlacementRule.check_positioned(&leaked);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took it");
    assert!(out[0].1.contains("100 Trying carries Record-Route"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the 100");
}

/// RFC 3581 §4 at the taker, and it never gates.
#[test]
fn a_dropped_rport_is_advisory_and_reported_at_the_taker() {
    assert!(RportEchoRule.force_advisory());
    let ask = |tail: &str| {
        format!(
            "OPTIONS {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let answer = |tail: &str| {
        format!(
            "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(""), BOB, 1)];
    let out = RportEchoRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that asked");
    assert!(out[0].1.contains("dropped the rport"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the response");

    let echoed = vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(";rport=5080"), BOB, 1)];
    assert!(RportEchoRule.check_positioned(&echoed).is_empty());
}

/// §13.2.1 / §20.37 at the taker: ONE finding names both headers.
#[test]
fn a_re_invite_with_no_capabilities_is_reported_once_at_the_taker() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("INVITE", "z9hG4bK-r", 2, Some("bt")), ALICE, 1),
    ];
    let out = AllowSupportedOnInviteRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "one finding, both headers on it: {out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took the re-INVITE");
    assert!(
        out[0].1.contains("missing Allow:") && out[0].1.contains("missing Supported:"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(2), "offending points at the re-INVITE");

    let advertised = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(
            SUT,
            in_dialog(
                "INVITE",
                B,
                "z9hG4bK-r",
                2,
                A,
                B,
                "Allow: INVITE, ACK\r\nSupported: 100rel\r\n",
            ),
            ALICE,
            1,
        ),
    ];
    assert!(AllowSupportedOnInviteRule.check_positioned(&advertised).is_empty());
}

/// §16.7 step 5 at the taker: one 100 per INVITE sent is owed, and only the
/// copy past the budget is reported — once, whatever the count.
#[test]
fn an_extra_100_is_reported_once_at_the_taker() {
    let owed = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 1),
    ];
    assert!(Proxy100TryingNotForwardedRule.check_positioned(&owed).is_empty());

    let mut extra = owed;
    extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 2));
    extra.push(recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 3));
    let out = Proxy100TryingNotForwardedRule.check_positioned(&extra);
    assert_eq!(out.len(), 1, "one finding for the transaction: {out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took them");
    assert!(out[0].1.contains("received 3 100 Trying"), "{}", out[0].1);
    assert!(out[0].1.contains("against 1 INVITE(s) sent"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the first excess 100");
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
            .filter(|f| f.rule == rfc_rules::RuleId::SingleFinalPerServerTxn.token())
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

// ---- the §8.2 / §11.2 / §12.2.2 / §16.3 reject-and-capability family ---

/// §8.2.1 at the taker: the lane that TOOK the unknown verb is the lane
/// charged, and the finding names what it answered instead.
#[test]
fn an_unrecognised_method_is_flagged_on_the_lane_that_took_it() {
    let clean = vec![
        recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
        sent(SUT, resp_with(405, 1, "FROBNICATE", "z9hG4bK-x", "Allow: INVITE, BYE\r\n"), ALICE, 1),
    ];
    assert!(UnsupportedMethod405AllowRule.check_positioned(&clean).is_empty());

    let served = vec![
        recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "FROBNICATE", "z9hG4bK-x", ""), ALICE, 1),
    ];
    let out = UnsupportedMethod405AllowRule.check_positioned(&served);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that took the verb owed the 405");
    assert!(out[0].1.contains("405") && out[0].1.contains("got 200 / Allow=0"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(1), "offending points at the request it answered wrong");
}

/// §8.2.2 at the taker: an unsupported `Require` served instead of refused,
/// with the tag named.
#[test]
fn an_unsupported_require_served_is_flagged() {
    let clean = vec![
        recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
        sent(
            SUT,
            resp_with(420, 1, "INVITE", "z9hG4bK-i", "Unsupported: frobnicate\r\n"),
            ALICE,
            1,
        ),
    ];
    assert!(UnsupportedExtension420Rule.check_positioned(&clean).is_empty());

    let served = vec![
        recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
        sent(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 1),
    ];
    let out = UnsupportedExtension420Rule.check_positioned(&served);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("[frobnicate]") && out[0].1.contains("420"), "{}", out[0].1);
}

/// §8.2.3 and §21.4.15 at the sender: the lane that SENT the bare final is
/// the lane charged.
#[test]
fn a_bare_415_and_a_bare_421_are_flagged_on_their_sender() {
    let bare_415 = vec![sent(SUT, resp_with(415, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
    let out = Unsupported415AcceptsRule.check_positioned(&bare_415);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Accept / Accept-Encoding / Accept-Language"), "{}", out[0].1);

    let with_accept = vec![sent(
        SUT,
        resp_with(415, 1, "INVITE", "z9hG4bK-i", "Accept: application/sdp\r\n"),
        ALICE,
        0,
    )];
    assert!(Unsupported415AcceptsRule.check_positioned(&with_accept).is_empty());

    let bare_421 = vec![sent(SUT, resp_with(421, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
    let out = UnsupportedExtension421Rule.check_positioned(&bare_421);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("Require"), "{}", out[0].1);

    let listed =
        vec![sent(SUT, resp_with(421, 1, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"), ALICE, 0)];
    assert!(UnsupportedExtension421Rule.check_positioned(&listed).is_empty());
}

/// §11.2 is advisory and reads at the sender of the 2xx.
#[test]
fn a_bare_options_200_is_advisory_at_its_sender() {
    assert!(OptionsResponseEchoesRule.force_advisory());
    let clean = vec![
        recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", "Allow: INVITE\r\n"), ALICE, 1),
    ];
    assert!(OptionsResponseEchoesRule.check_positioned(&clean).is_empty());

    let bare = vec![
        recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", ""), ALICE, 1),
    ];
    let out = OptionsResponseEchoesRule.check_positioned(&bare);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Allow/Supported/Accept"), "{}", out[0].1);
}

/// §12.2.2 at the taker: a BYE naming a peer tag this lane never confirmed,
/// served instead of refused 481.
#[test]
fn a_request_for_an_unknown_dialog_is_flagged_on_the_lane_that_served_it() {
    /// A BYE alice sent the SUT, with a caller-chosen From tag.
    fn bye(from_tag: &str) -> Vec<u8> {
        format!(
            "BYE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-b\r\n\
                 From: <{A}>;tag={from_tag}\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 2 BYE\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }
    let confirmed = |from_tag: &str, bye_status: u16| {
        vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
            recv(SUT, bye(from_tag), ALICE, 3),
            sent(SUT, resp(bye_status, 2, "BYE", "bt", "z9hG4bK-b"), ALICE, 4),
        ]
    };
    assert!(
        UnknownDialog481Rule.check_positioned(&confirmed("at", 200)).is_empty(),
        "the dialog the 200 confirmed is known"
    );
    assert!(
        UnknownDialog481Rule.check_positioned(&confirmed("zz", 481)).is_empty(),
        "the 481 discharges it"
    );
    let out = UnknownDialog481Rule.check_positioned(&confirmed("zz", 200));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that owed the 481 is charged");
    assert!(out[0].1.contains("481") && out[0].1.contains("answered 200"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the BYE it served");
}

/// §16.3: advisory, `{Proxy}`-only, and — the regression this rule exists
/// for — a §16.6 forward on a FRESH branch is a resolved target, not a
/// no-target outcome.
#[test]
fn no_target_404_is_advisory_proxy_scoped_and_survives_a_branch_rewrite() {
    assert!(NoTarget404Rule.force_advisory());
    assert_eq!(NoTarget404Rule.subject(), HashSet::from([UaRole::Proxy]));

    let unforwarded = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, resp(500, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
    ];
    let out = NoTarget404Rule.check_positioned(&unforwarded);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("expected 404"), "{}", out[0].1);

    let answered_404 = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, resp(404, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 1),
    ];
    assert!(NoTarget404Rule.check_positioned(&answered_404).is_empty());

    let forwarded = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-out", 1, None), BOB, 1),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-out"), BOB, 2),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 3),
    ];
    assert!(
        NoTarget404Rule.check_positioned(&forwarded).is_empty(),
        "{:?}",
        NoTarget404Rule.check_positioned(&forwarded)
    );

    let relayed = vec![
        recv(SUT, req("INVITE", "z9hG4bK-in", 1, None), ALICE, 0),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-other"), BOB, 1),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-in"), ALICE, 2),
    ];
    assert!(
        NoTarget404Rule.check_positioned(&relayed).is_empty(),
        "a relayed downstream rejection resolved a target"
    );
}

/// §13.2.2.4 / §17.1.1.3 at the emitter: the lane that SENT the ACK is the
/// lane charged, and the position pins the ACK itself.
#[test]
fn a_divergent_ack_is_flagged_on_the_lane_that_sent_it() {
    let evs = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
        sent(SUT, req_with("ACK", "z9hG4bK-i", "Require: timer\r\n"), BOB, 2),
    ];
    let routes = AckPreservesInviteRouteRule.check_positioned(&evs);
    assert_eq!(routes.len(), 1, "{routes:?}");
    assert_eq!(routes[0].0, SUT, "attributed to the lane that sent the ACK");
    assert!(routes[0].1.contains("differ"), "{}", routes[0].1);
    assert_eq!(routes[0].2, Some(3), "offending points at the ACK");

    let requires = AckRequireSubsetOfInviteRule.check_positioned(&evs);
    assert_eq!(requires.len(), 1, "{requires:?}");
    assert!(requires[0].1.contains("subset"), "{}", requires[0].1);
    assert_eq!(requires[0].2, Some(3));

    // The ACK that echoes its INVITE surfaces nothing at all.
    let clean = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-i", "Require: 100rel\r\n"), BOB, 0),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), BOB, 1),
        sent(SUT, req_with("ACK", "z9hG4bK-i", "Require: 100rel\r\n"), BOB, 2),
    ];
    assert!(AckPreservesInviteRouteRule.check_positioned(&clean).is_empty());
    assert!(AckRequireSubsetOfInviteRule.check_positioned(&clean).is_empty());
}

#[test]
fn relay_lane_passing_an_ack_through_is_not_judged() {
    // A lane that forwards both directions of one Call-ID passes the
    // upstream UAC's ACK through, Route and Require included.
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req_with("INVITE", "z9hG4bK-b", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
        recv(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 2),
        sent(SUT, req_with("ACK", "z9hG4bK-b", "Require: timer\r\n"), BOB, 3),
    ];
    assert!(AckPreservesInviteRouteRule.check_positioned(&evs).is_empty());
    assert!(AckRequireSubsetOfInviteRule.check_positioned(&evs).is_empty());
}

/// §16.4 is proxy behaviour: a `{Proxy}`-declared lane is judged, a UA lane
/// is not — the subject, not the body, is what keeps a UA off this rule.
#[test]
fn strict_route_is_judged_only_on_a_declared_proxy_lane() {
    let wire =
        vec![recv(SUT, req_with("INVITE", "z9hG4bK-in", "Route: <sip:strict@h>\r\n"), ALICE, 1)];
    let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
    proxy.extend(wire.clone());
    let found: Vec<RfcFinding> = evaluate_rfc_findings(&proxy)
        .into_iter()
        .filter(|f| f.rule == StrictRouteRewriteHandledRule.name())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lane, SUT);
    assert!(found[0].detail.contains("strict-route"), "{}", found[0].detail);
    assert_eq!(found[0].offending, Some(1), "offending points at the request taken");

    let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
    ua.extend(wire);
    assert!(
        !evaluate_rfc_findings(&ua).iter().any(|f| f.rule == StrictRouteRewriteHandledRule.name()),
        "a UA lane forwards nothing and is not judged",
    );
}

/// A REGISTER the lane SENT, carrying a Route header — §10.2 owes none.
#[test]
fn a_register_carrying_route_is_flagged_on_the_lane_that_sent_it() {
    let evs = vec![sent(SUT, req_with("REGISTER", "z9hG4bK-r", "Route: <sip:p@h;lr>\r\n"), BOB, 0)];
    let out = RegisterNoRouteSetRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Route"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(1), "offending points at the REGISTER");

    let clean = vec![sent(SUT, req_with("REGISTER", "z9hG4bK-r", ""), BOB, 0)];
    assert!(RegisterNoRouteSetRule.check_positioned(&clean).is_empty());
}

/// §10.2 serialisation live: a second binding for one AOR while the first
/// is unanswered, reported on the sending lane.
#[test]
fn a_racing_register_is_flagged_on_the_lane_that_sent_it() {
    let register = |branch: &str, contact: &str| {
        format!(
            "REGISTER {A} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{A}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 REGISTER\r\n\
                 Contact: {contact}\r\n\
                 Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
        sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 1),
    ];
    let out = SerialRegisterRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("still pending"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the second REGISTER");

    // Answered first: the next binding is free to differ.
    let serialised = vec![
        sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
        recv(SUT, resp(200, 1, "REGISTER", "rt", "z9hG4bK-1"), BOB, 1),
        sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 2),
    ];
    assert!(SerialRegisterRule.check_positioned(&serialised).is_empty());
}

/// Every merged rule reaches the live suite. A rung that gives a rule a
/// body and forgets to surface it here would otherwise run nowhere and say
/// nothing — the adapter is the ONLY live consumer, so the two registries
/// are one fact stated twice.
///
/// One token may carry SEVERAL adapters: a live policy that waives part of
/// an obligation states the waived arm and the gating arm separately, and
/// they answer to the same rule. Two adapters agreeing on severity are a
/// duplicate, not a split, and the rung says so.
#[test]
fn every_merged_rule_has_a_live_adapter() {
    let mut by_token: std::collections::BTreeMap<&str, Vec<bool>> =
        std::collections::BTreeMap::new();
    for rule in cross_rules() {
        by_token.entry(rule.name()).or_default().push(rule.force_advisory());
    }
    for (token, mut severities) in by_token.clone() {
        let arms = severities.len();
        severities.sort_unstable();
        severities.dedup();
        assert_eq!(arms, severities.len(), "{token} is surfaced twice at one severity");
    }
    let surfaced: Vec<&str> = by_token.keys().copied().collect();
    let mut declared: Vec<&str> = rfc_rules::RuleId::ALL.iter().map(|r| r.token()).collect();
    declared.sort_unstable();
    assert_eq!(surfaced, declared);
}

// ── the §14 / §15 / §16.7 / §17.1.1.3 family ────────────────────────────

/// A request the CALLEE originates: From/To reversed relative to the
/// establishing INVITE, which is the orientation a callee-initiated BYE or
/// re-INVITE rides.
fn from_callee(method: &str, branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "{method} {A} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
             From: <{B}>;tag=bt\r\n\
             To: <{A}>;tag=at\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// §13.3.1.4 through the projection: the ACK discharges the UAS's 2xx, a
/// BYE on the dialog discharges it too, and a retransmitted 2xx re-opens
/// nothing. The leak — neither ACKed nor BYE'd — GATES.
#[test]
fn unacked_2xx_is_discharged_by_the_ack_or_a_bye_and_otherwise_gates() {
    let acked = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(BOB, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 2),
    ];
    assert!(Unacked2xxNotClearedRule.check(&acked).is_empty());

    let byed = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        // A retransmit of the same 2xx must not re-open the obligation.
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
        sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 3),
    ];
    assert!(Unacked2xxNotClearedRule.check(&byed).is_empty());

    let leaked = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
    ];
    let f = Unacked2xxNotClearedRule.check(&leaked);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("never ACKed"), "{}", f[0].1);
    assert!(!Unacked2xxNotClearedRule.force_advisory(), "the leak gates");
}

/// §14.2 at the UAS lane: the racing re-INVITE surfaces on the lane that
/// TOOK it, pointed at that message. Its sender's earlier INVITE is the
/// pending one, so 500 with Retry-After surfaces nothing and 491 surfaces.
#[test]
fn a_racing_re_invite_surfaces_at_the_lane_that_took_it() {
    let evs = vec![
        recv(BOB, req("INVITE", "z9hG4bK-1", 2, Some("bt")), ALICE, 0),
        recv(BOB, req("INVITE", "z9hG4bK-2", 3, Some("bt")), ALICE, 1),
        sent(BOB, resp(200, 3, "INVITE", "bt", "z9hG4bK-2"), ALICE, 2),
    ];
    let out = ConcurrentReInvite500Or491Rule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "charged to the UAS that owed the 500");
    assert!(out[0].1.contains("500+Retry-After"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the racing re-INVITE");

    let answered = |status: u16, retry_after: &str| {
        let refusal = String::from_utf8(resp(status, 3, "INVITE", "bt", "z9hG4bK-2"))
            .expect("utf-8")
            .replace("Content-Length:", &format!("{retry_after}Content-Length:"));
        vec![
            recv(BOB, req("INVITE", "z9hG4bK-1", 2, Some("bt")), ALICE, 0),
            recv(BOB, req("INVITE", "z9hG4bK-2", 3, Some("bt")), ALICE, 1),
            sent(BOB, refusal.into_bytes(), ALICE, 2),
            sent(BOB, resp(200, 2, "INVITE", "bt", "z9hG4bK-1"), ALICE, 3),
        ]
    };
    let compliant = answered(500, "Retry-After: 4\r\n");
    assert!(ConcurrentReInvite500Or491Rule.check_positioned(&compliant).is_empty());
    let pending_491 = answered(491, "");
    assert_eq!(ConcurrentReInvite500Or491Rule.check_positioned(&pending_491).len(), 1);
}

/// §15 at the sending lane: a BYE with no peer tag is off-dialog, and a
/// callee that BYEs before it has accepted is the early-dialog shape.
#[test]
fn a_bye_off_dialog_and_an_early_callee_bye_both_surface() {
    let off = vec![sent(ALICE, req("BYE", "z9hG4bK-b", 2, None), BOB, 0)];
    let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&off);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, ALICE);
    assert!(f[0].1.contains("outside any dialog"), "{}", f[0].1);

    let early = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 2),
    ];
    let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&early);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, BOB, "the callee is charged");
    assert!(f[0].1.contains("early dialog"), "{}", f[0].1);

    // Accepted first: the same BYE is the ordinary teardown.
    let accepted = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(BOB, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 2),
        sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 3),
    ];
    assert!(NoByeOutsideOrEarlyDialogRule.check_positioned(&accepted).is_empty());
}

/// §14.1 at the sending lane: the re-INVITE that overtook its own prior
/// transaction's ACK names the RFC 6026 phase it collided with.
#[test]
fn a_re_invite_overtaking_the_ack_surfaces_at_the_uac_lane() {
    let evs = vec![
        sent(ALICE, req("INVITE", "z9hG4bK-1", 2, Some("bt")), BOB, 0),
        recv(ALICE, resp(200, 2, "INVITE", "bt", "z9hG4bK-1"), BOB, 1),
        sent(ALICE, req("INVITE", "z9hG4bK-2", 3, Some("bt")), BOB, 2),
    ];
    let out = NoReInviteWhileInviteInProgressRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("not yet ACKed"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the overtaking re-INVITE");
}

/// §16.7 stays ADVISORY and proxy-scoped: a paused test clock advances
/// virtual time no real latency corresponds to.
#[test]
fn the_100_trying_grace_is_advisory_and_proxy_scoped() {
    assert!(Proxy100WithinGraceRule.force_advisory());
    assert_eq!(Proxy100WithinGraceRule.subject(), HashSet::from([UaRole::Proxy]));

    let silent = vec![recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0)];
    let f = Proxy100WithinGraceRule.check_positioned(&silent);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("100 Trying") && f[0].1.contains("no response sent"), "{}", f[0].1);

    // Answered inside the grace: §16.7 owes nothing.
    let prompt = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 5),
    ];
    assert!(Proxy100WithinGraceRule.check_positioned(&prompt).is_empty());

    // Past it, the finding names the Δ actually observed.
    let late = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 350),
    ];
    let f = Proxy100WithinGraceRule.check_positioned(&late);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("Δ=350ms"), "{}", f[0].1);
}

/// §17.1.1.3 GATES, reported at the rejecting UAS lane where the ACK had to
/// arrive and charged to the UAC that owed it; a 2xx is another rule's
/// business.
#[test]
fn an_unacked_reject_gates_at_the_uas_lane() {
    let evs = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
    ];
    let out = UnackedInviteNon2xxFinalRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB);
    assert_eq!(out[0].3, Some(ALICE.to_string()), "charged to the UAC that owed the ACK");
    assert!(out[0].1.contains("never ACKed"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the un-ACKed final");
    assert!(!UnackedInviteNon2xxFinalRule.force_advisory(), "promoted to gating");

    let acked = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(BOB, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
    ];
    assert!(UnackedInviteNon2xxFinalRule.check_positioned(&acked).is_empty());

    // A recording holding only the UAC's lane names the withheld ACK there.
    let uac_side = vec![
        sent(ALICE, req("INVITE", "z9hG4bK-u", 1, None), BOB, 0),
        recv(ALICE, resp(486, 1, "INVITE", "bt", "z9hG4bK-u"), BOB, 1),
    ];
    let out = UnackedInviteNon2xxFinalRule.check_positioned(&uac_side);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!((out[0].0.as_str(), out[0].3.as_deref()), (ALICE, Some(ALICE)));

    // Both lanes recorded: one finding, at the UAS's.
    let both = vec![
        sent(ALICE, req("INVITE", "z9hG4bK-b", 1, None), BOB, 0),
        recv(BOB, req("INVITE", "z9hG4bK-b", 1, None), ALICE, 1),
        sent(BOB, resp(486, 1, "INVITE", "bt", "z9hG4bK-b"), ALICE, 2),
        recv(ALICE, resp(486, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 3),
    ];
    let out = UnackedInviteNon2xxFinalRule.check_positioned(&both);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!((out[0].0.as_str(), out[0].3.as_deref()), (BOB, Some(ALICE)));
}

/// §14.1's abandoned transaction: a provisional and then silence on a
/// dialog nothing ever BYE'd, surfaced at the lane that sent the re-INVITE.
#[test]
fn an_abandoned_re_invite_surfaces_unless_the_dialog_was_byed() {
    let evs = vec![
        sent(ALICE, req("INVITE", "z9hG4bK-re", 2, Some("bt")), BOB, 0),
        recv(ALICE, resp(183, 2, "INVITE", "bt", "z9hG4bK-re"), BOB, 1),
    ];
    let out = FailedReinviteTearsDownDialogRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("silently torn down"), "{}", out[0].1);
    assert!(!FailedReinviteTearsDownDialogRule.force_advisory());

    // "Absent another cause": a BYE is an independent teardown.
    let byed = vec![
        sent(ALICE, req("INVITE", "z9hG4bK-re", 2, Some("bt")), BOB, 0),
        recv(ALICE, resp(100, 2, "INVITE", "", "z9hG4bK-re"), BOB, 1),
        sent(ALICE, req("BYE", "z9hG4bK-bye", 3, Some("bt")), BOB, 2),
    ];
    assert!(FailedReinviteTearsDownDialogRule.check_positioned(&byed).is_empty());
}

/// §13.3.1.1 at the UAS lane: a NEW provisional after the final fires and
/// points at itself; a relay lane forwarding its upstream's is not judged.
#[test]
fn a_late_provisional_fires_at_the_uas_lane_and_not_on_a_relay() {
    assert_eq!(No1xxAfterFinalRule.subject(), HashSet::from([UaRole::Uas]));
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
        sent(SUT, resp(181, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 4),
    ];
    let out = No1xxAfterFinalRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the UAS lane");
    assert_eq!(out[0].2, Some(5), "offending points at the late 181");

    // A retransmitted final and a reordered pre-final provisional are not
    // new emissions.
    let retransmits = vec![
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 3),
    ];
    assert!(No1xxAfterFinalRule.check_positioned(&retransmits).is_empty());

    // A lane that forwards both directions of one Call-ID relays what the
    // upstream produced; the late 1xx is flagged on the upstream lane.
    let relayed = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
        sent(SUT, resp(183, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
    ];
    assert!(No1xxAfterFinalRule.check_positioned(&relayed).is_empty());
}

// ── the RFC 3262 family ─────────────────────────────────────────────────
//
// Fixtures that can carry the reliable-provisional headers, an `RAck` and a
// body: the whole family reads those, and the `Content-Length` is what a
// rule asks about a body from a head-only vantage.

const SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n\
                       m=audio 5004 RTP/AVP 0\r\n";

fn body_rows(sdp: bool) -> String {
    match sdp {
        true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
        false => "Content-Length: 0\r\n".to_string(),
    }
}

/// The body bytes those rows describe — the live adapter parses the whole
/// datagram, so a declared length with nothing behind it is not a message.
fn body(sdp: bool) -> &'static str {
    if sdp {
        SDP
    } else {
        ""
    }
}

/// The reliable-provisional markers RFC 3262 §3 requires of both halves.
fn reliable_rows(rseq: u64) -> String {
    format!("Require: 100rel\r\nRSeq: {rseq}\r\n")
}

/// An INVITE with caller-chosen option-tag rows, To tag and body.
fn invite_3262(branch: &str, extra: &str, to_tag: Option<&str>, sdp: bool) -> Vec<u8> {
    let to = match to_tag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n{extra}{}\r\n{}",
        body_rows(sdp),
        body(sdp),
    )
    .into_bytes()
}

/// An INVITE-transaction response with caller-chosen rows and body.
fn inv_resp_3262(status: u16, branch: &str, extra: &str, sdp: bool) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n{extra}{}\r\n{}",
        body_rows(sdp),
        body(sdp),
    )
    .into_bytes()
}

/// An in-dialog UPDATE (RFC 3311) and a response on its transaction — a
/// non-INVITE request, which no reliable provisional may answer.
fn update_3262(branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "UPDATE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 UPDATE\r\n\
             Max-Forwards: 70\r\n{extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn update_resp_3262(status: u16, branch: &str, extra: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 UPDATE\r\n{extra}Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A PRACK whose `RAck` is spelled out verbatim.
fn prack_3262(branch: &str, rack: &str, sdp: bool) -> Vec<u8> {
    format!(
        "PRACK {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n{}\r\n{}",
        body_rows(sdp),
        body(sdp),
    )
    .into_bytes()
}

/// The response on that PRACK's own server transaction.
fn prack_resp_3262(status: u16, branch: &str) -> Vec<u8> {
    resp(status, 2, "PRACK", "bt", branch)
}

// ── the obligation half ─────────────────────────────────────────────────

#[test]
fn a_second_reliable_provisional_waits_for_the_prack_of_the_last() {
    let serialised = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
        sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 3),
    ];
    assert!(NoOverlappingReliableProvisionalsRule.check(&serialised).is_empty());

    let overlapped = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 1),
    ];
    let out = NoOverlappingReliableProvisionalsRule.check(&overlapped);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-012"), "{}", out[0].1);
}

#[test]
fn an_rseq_gap_is_flagged_at_the_offending_provisional() {
    let contiguous = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(5), false), ALICE, 0),
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(6), false), ALICE, 1),
    ];
    assert!(NonContiguousRseqRule.check(&contiguous).is_empty());

    let gapped = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(5), false), ALICE, 0),
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(8), false), ALICE, 1),
    ];
    let out = NonContiguousRseqRule.check_positioned(&gapped);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-013"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending = the non-contiguous 183");
}

#[test]
fn a_reliable_provisional_the_uac_never_pracked_is_flagged() {
    let pracked = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
        sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
    ];
    assert!(UnackedReliableProvisionalRule.check(&pracked).is_empty());

    let unpracked = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
    ];
    let out = UnackedReliableProvisionalRule.check(&unpracked);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-021"), "{}", out[0].1);

    // The merged rule needs the 100rel negotiation WITNESSED: a provisional
    // whose INVITE this vantage never carried settles nothing.
    let unwitnessed =
        vec![recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0)];
    assert!(UnackedReliableProvisionalRule.check(&unwitnessed).is_empty());
}

/// RFC 3262 §4 live: a reliable 180 the SUT takes after its own CANCEL,
/// the 487 two seconds later, is owed its PRACK — the CANCEL ends nothing.
#[test]
fn a_reliable_provisional_after_the_cancel_still_owes_its_prack() {
    let cancel = format!(
        "CANCEL {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 CANCEL\r\n\
             Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes();
    let events = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
        sent(SUT, cancel, BOB, 50),
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 100),
        recv(SUT, inv_resp_3262(487, "z9hG4bK-i", "", false), BOB, 2_100),
    ];
    let out = UnackedReliableProvisionalRule.check(&events);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-021"), "{}", out[0].1);
}

#[test]
fn pracking_out_of_order_is_flagged_at_the_prack() {
    let in_order = vec![
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
        sent(SUT, prack_3262("z9hG4bK-p1", "1 1 INVITE", false), BOB, 1),
        recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), BOB, 2),
        sent(SUT, prack_3262("z9hG4bK-p2", "2 1 INVITE", false), BOB, 3),
    ];
    assert!(NoPrackOfOutOfOrderRseqRule.check(&in_order).is_empty());

    let jumped = vec![
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
        recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(5), false), BOB, 1),
        sent(SUT, prack_3262("z9hG4bK-p", "5 1 INVITE", false), BOB, 2),
    ];
    let out = NoPrackOfOutOfOrderRseqRule.check_positioned(&jumped);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-024"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending = the PRACK, not the 1xx");
}

// ── the §3/§4/§5 negotiation half ───────────────────────────────────────

#[test]
fn a_plain_18x_against_require_100rel_is_flagged_at_its_wire_position() {
    let evs = vec![
        recv(SUT, invite_3262("z9hG4bK-i", "Require: 100rel\r\n", None, false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", "", false), ALICE, 1),
        sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
    ];
    let out = RequireReliable1xxOnRequireRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("MUST-001/-002"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the plain 180");

    // Rejecting the extension is §3's other half: nothing to answer for.
    let rejected = vec![
        recv(SUT, invite_3262("z9hG4bK-i", "Require: 100rel\r\n", None, false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", "", false), ALICE, 1),
        sent(SUT, inv_resp_3262(420, "z9hG4bK-i", "Unsupported: 100rel\r\n", false), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
    ];
    assert!(RequireReliable1xxOnRequireRule.check(&rejected).is_empty());
}

#[test]
fn a_relay_lane_is_not_judged_on_the_reliable_provisional_negotiation() {
    // A lane that forwards both directions of one Call-ID relays what the
    // upstream UAS produced; the plain 18x is the upstream's emission.
    let evs = vec![
        recv(SUT, invite_3262("z9hG4bK-a", "Require: 100rel\r\n", None, false), ALICE, 0),
        sent(SUT, invite_3262("z9hG4bK-b", "Require: 100rel\r\n", None, false), BOB, 1),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-a", "", false), ALICE, 2),
    ];
    assert!(RequireReliable1xxOnRequireRule.check(&evs).is_empty());
}

#[test]
fn a_reliable_1xx_without_client_opt_in_is_advisory_on_the_uas_lane() {
    assert!(ReliableNeedsClientOptInRule.force_advisory());
    assert_eq!(
        ReliableNeedsClientOptInRule.subject(),
        HashSet::from([UaRole::Uas]),
        "the UAS is the party §3 binds here",
    );
    let evs = vec![
        recv(SUT, invite_3262("z9hG4bK-i", "", None, false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
    ];
    let out = ReliableNeedsClientOptInRule.check(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-004"), "{}", out[0].1);

    let opted = vec![
        recv(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
    ];
    assert!(ReliableNeedsClientOptInRule.check(&opted).is_empty());
}

/// A re-INVITE is an INVITE, so a reliable provisional to one is clean at
/// this bind too (RFC 3262 §3 scopes the mechanism by method; its To-tag
/// bar is the proxy's, "unlike a UAS").
#[test]
fn a_reliable_1xx_answering_a_re_invite_is_clean() {
    let evs = vec![
        recv(SUT, invite_3262("z9hG4bK-re", "Supported: 100rel\r\n", Some("bt"), false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-re", &reliable_rows(1), false), ALICE, 1),
    ];
    assert!(NoReliable1xxOnInDialogRule.check(&evs).is_empty());

    let initial = vec![
        recv(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), ALICE, 0),
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 1),
    ];
    assert!(NoReliable1xxOnInDialogRule.check(&initial).is_empty());

    // The offence the rule is for: a reliable provisional to an UPDATE.
    let update = vec![
        recv(SUT, update_3262("z9hG4bK-u", "Supported: 100rel\r\n"), ALICE, 0),
        sent(SUT, update_resp_3262(183, "z9hG4bK-u", &reliable_rows(1)), ALICE, 1),
    ];
    let out = NoReliable1xxOnInDialogRule.check(&update);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-005") && out[0].1.contains("UPDATE"), "{}", out[0].1);
}

#[test]
fn a_prack_absorbed_by_a_proxy_bind_is_advisory_on_that_bind() {
    assert!(UnmatchedPrackProxiedRule.force_advisory());
    assert_eq!(UnmatchedPrackProxiedRule.subject(), HashSet::from([UaRole::Proxy]));
    let evs = vec![
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "9 1 INVITE", false), ALICE, 1),
    ];
    let out = UnmatchedPrackProxiedRule.check(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "charged to the bind that took the PRACK");
    assert!(out[0].1.contains("MUST-006"), "{}", out[0].1);

    let matched = vec![
        recv(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), BOB, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
    ];
    assert!(UnmatchedPrackProxiedRule.check(&matched).is_empty());
}

#[test]
fn a_prack_draws_2xx_on_a_match_and_481_without_one() {
    let matched_but_rejected = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
        sent(SUT, prack_resp_3262(481, "z9hG4bK-p"), ALICE, 2),
    ];
    let out = Prack2xxOr481Rule.check(&matched_but_rejected);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-009"), "{}", out[0].1);

    let unmatched_but_accepted = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "9 1 INVITE", false), ALICE, 1),
        sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
    ];
    let out = Prack2xxOr481Rule.check(&unmatched_but_accepted);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-010"), "{}", out[0].1);

    let honoured = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
        sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
    ];
    assert!(Prack2xxOr481Rule.check(&honoured).is_empty());
}

#[test]
fn the_2xx_over_an_unacked_reliable_offer_is_flagged_at_the_2xx() {
    let early = vec![
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
        sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
    ];
    let out = Delay2xxOnUnackedReliable1xxWithSdpRule.check_positioned(&early);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-014") && out[0].1.contains("RSeq=1"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the 2xx");

    let waited = vec![
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", true), ALICE, 1),
        sent(SUT, prack_resp_3262(200, "z9hG4bK-p"), ALICE, 2),
        sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 3),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 4),
    ];
    assert!(Delay2xxOnUnackedReliable1xxWithSdpRule.check(&waited).is_empty());
}

#[test]
fn a_new_reliable_1xx_after_the_final_is_flagged_and_a_retransmit_is_not() {
    let stray = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(2), false), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 3),
    ];
    let out = NoNewReliable1xxAfterFinalRule.check_positioned(&stray);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-016") && out[0].1.contains("RSeq=2"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the stray 183");

    let clean = vec![
        sent(SUT, inv_resp_3262(180, "z9hG4bK-i", &reliable_rows(1), false), ALICE, 0),
        sent(SUT, inv_resp_3262(200, "z9hG4bK-i", "", false), ALICE, 1),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
    ];
    assert!(NoNewReliable1xxAfterFinalRule.check(&clean).is_empty());
}

#[test]
fn pracking_a_100_trying_is_flagged_at_the_prack() {
    let evs = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
        recv(SUT, inv_resp_3262(100, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
        sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
    ];
    let out = NoPrackOf100TryingRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-019"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the PRACK");

    let ignored = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, false), BOB, 0),
        recv(SUT, inv_resp_3262(100, "z9hG4bK-i", &reliable_rows(1), false), BOB, 1),
    ];
    assert!(NoPrackOf100TryingRule.check(&ignored).is_empty());
}

#[test]
fn a_bodiless_prack_for_a_1xx_offer_is_named_at_either_end() {
    assert!(!PrackAnswers1xxOfferRule.force_advisory(), "a bodiless PRACK to an offer gates");
    assert_eq!(PrackAnswers1xxOfferRule.subject(), HashSet::from([UaRole::Uac, UaRole::Uas]));
    // The bind SENT the offending PRACK: charged party and vantage are one.
    let sender = vec![
        recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), BOB, 0),
        sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 1),
    ];
    let out = PrackAnswers1xxOfferRule.check(&sender);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-025"), "{}", out[0].1);

    // The bind only TOOK it: the offender is no recorded bind at all, and
    // the recording is the only place that stream is checked.
    let taker = vec![
        sent(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), ALICE, 0),
        recv(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), ALICE, 1),
    ];
    let out = PrackAnswers1xxOfferRule.check(&taker);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);

    // The standard setup: the INVITE carried the offer, so the 183's body
    // is the ANSWER and the bodiless PRACK owes nothing.
    let standard = vec![
        sent(SUT, invite_3262("z9hG4bK-i", "Supported: 100rel\r\n", None, true), BOB, 0),
        recv(SUT, inv_resp_3262(183, "z9hG4bK-i", &reliable_rows(1), true), BOB, 1),
        sent(SUT, prack_3262("z9hG4bK-p", "1 1 INVITE", false), BOB, 2),
    ];
    assert!(PrackAnswers1xxOfferRule.check(&standard).is_empty());
}

// ── the §13.2 / RFC 3264 offer/answer family ────────────────────────────

const AUDIO_OFFER: &str = "v=0\r\n\
o=alice 424242 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 27500 RTP/AVP 8\r\n";

const AUDIO_ANSWER: &str = "v=0\r\n\
o=bob 117241 0 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=audio 60788 RTP/AVP 8\r\n";

const T38_OFFER: &str = "v=0\r\n\
o=alice 424242 2 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=image 27500 udptl t38\r\n";

const T38_ANSWER: &str = "v=0\r\n\
o=bob 117241 1 IN IP4 10.0.0.2\r\n\
s=-\r\n\
c=IN IP4 10.0.0.2\r\n\
t=0 0\r\n\
m=image 60788 udptl t38\r\n";

/// The harness default body that must never ride an ACK: a fresh session
/// (`o=bob 1 1`) offering audio over RTP/AVP.
const STRAY_ANSWER: &str = "v=0\r\n\
o=bob 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 20000 RTP/AVP 0\r\n";

/// A request carrying an optional SDP body.
fn req_sdp(
    method: &str,
    branch: &str,
    cseq: u32,
    ttag: Option<&str>,
    body: Option<&str>,
) -> Vec<u8> {
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    let body = body.unwrap_or("");
    let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let mut v = format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    v.extend_from_slice(body.as_bytes());
    v
}

/// A response carrying an optional SDP body.
fn resp_sdp(status: u16, cseq: u32, method: &str, branch: &str, body: Option<&str>) -> Vec<u8> {
    let body = body.unwrap_or("");
    let ctype = if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" };
    let mut v = format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             {ctype}Content-Length: {}\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    v.extend_from_slice(body.as_bytes());
    v
}

/// INVITE(offer) → 180(answer) → 200 → ACK, then a T.38 re-INVITE(offer) →
/// 200(answer) → ACK, all from the CALLER's bind. The ACK bodies are the
/// caller's choice.
fn t38_call(
    first_ack: Option<&str>,
    second_ack: Option<&str>,
) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        recv(ALICE, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), BOB, 1),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 2),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), first_ack), BOB, 3),
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 4),
        recv(ALICE, resp_sdp(200, 3, "INVITE", "z9hG4bK-i3", Some(T38_ANSWER)), BOB, 5),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a3", 3, Some("bt"), second_ack), BOB, 6),
    ]
}

#[test]
fn an_ack_body_on_a_completed_round_is_flagged_at_its_wire_position() {
    assert!(AckBodyAfterCompleteOfferAnswerRule.check(&t38_call(None, None)).is_empty());

    let out =
        AckBodyAfterCompleteOfferAnswerRule.check_positioned(&t38_call(None, Some(STRAY_ANSWER)));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "charged to the lane that sent the ACK");
    assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§13.2.2.4"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(7), "offending points at the ACK that carried the body");

    // The establishing round is closed by the 180, not by the bodyless 200.
    let first = AckBodyAfterCompleteOfferAnswerRule.check(&t38_call(Some(STRAY_ANSWER), None));
    assert_eq!(first.len(), 1, "{first:?}");
}

#[test]
fn a_delayed_offer_ack_carries_the_answer_and_is_clean() {
    // INVITE without SDP → the 2xx holds the offer → the ACK holds the answer.
    let evs = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), Some(AUDIO_ANSWER)), BOB, 2),
    ];
    assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
}

/// The same as [`resp_sdp`], made reliable: `Require: 100rel` and an
/// `RSeq`, so its description binds (RFC 3262 §5).
fn reliable_resp_sdp(status: u16, cseq: u32, branch: &str, body: Option<&str>) -> Vec<u8> {
    let plain = resp_sdp(status, cseq, "INVITE", branch, body);
    let text = String::from_utf8(plain).unwrap();
    text.replacen("CSeq:", "Require: 100rel\r\nRSeq: 1\r\nCSeq:", 1).into_bytes()
}

/// §12.2.1.1 GATES: a request the caller takes long after a forking
/// callee's answer, under the tag of the branch that only rang, is flagged
/// at the caller's lane and charged to its sender.
#[test]
fn a_request_under_a_rung_branch_tag_after_the_answer_gates() {
    assert!(!InDialogFromTagRule.force_advisory(), "a MUST gates");
    let toward_alice = |from_tag: &str| {
        format!(
            "OPTIONS {A} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-o1\r\n\
                 From: <{B}>;tag={from_tag}\r\n\
                 To: <{A}>;tag=at\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 OPTIONS\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let call = |from_tag: &str| {
        vec![
            sent(ALICE, req("INVITE", "z9hG4bK-i1", 1, None), BOB, 0),
            recv(ALICE, resp(180, 1, "INVITE", "b1", "z9hG4bK-i1"), BOB, 1),
            recv(ALICE, resp(200, 1, "INVITE", "b2", "z9hG4bK-i1"), BOB, 2),
            sent(ALICE, req("ACK", "z9hG4bK-a1", 1, Some("b2")), BOB, 3),
            recv(ALICE, toward_alice(from_tag), BOB, 300_000),
        ]
    };
    assert!(InDialogFromTagRule.check(&call("b2")).is_empty());
    let out = InDialogFromTagRule.check_positioned(&call("b1"));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "reported at the caller, whose dialog it fails to name");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the request's sender");
}

/// §13.2.2.4 GATES: an ACK to a delayed-offer 2xx that carries no answer
/// is flagged at the 2xx sender's lane, charged to the ACK's sender.
#[test]
fn a_delayed_offer_acked_without_an_answer_gates() {
    assert!(!DelayedOfferAnsweredInAckRule.force_advisory(), "a MUST gates");
    let answered = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), Some(AUDIO_ANSWER)), ALICE, 2),
    ];
    assert!(DelayedOfferAnsweredInAckRule.check(&answered).is_empty());
    let silent = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, None), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_OFFER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = DelayedOfferAnsweredInAckRule.check_positioned(&silent);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported where the answer had to arrive");
    assert_eq!(out[0].3, Some(ALICE.to_string()), "charged to the ACK's sender");
}

/// An arrival of `raw` at `bind` with the given disposition and §17.2
/// repeat mark, as the recording stamps it at production.
fn arrival(
    bind: &str,
    raw: Vec<u8>,
    src: &str,
    seq: u64,
    disposition: crate::types::RecvDisposition,
    repeat_of: Option<u64>,
) -> Stamped<SignalingNetworkEvent> {
    let mut wire = crate::contracts::WireStamp::of_bytes(&raw);
    wire.repeat_of = repeat_of;
    Stamped {
        event: SignalingNetworkEvent::RecvItem {
            bind_key: bind.to_string(),
            disposition,
            wire,
            packet: UdpPacket { raw, src: src.parse().unwrap(), arrival_ms: seq },
        },
        seq,
        at_ms: seq,
    }
}

/// The audit view's repeat mark is relative to what the view holds: when
/// the loss model dropped the first copy of an offer, the retransmission is
/// the first copy the endpoint took, so its offer stands and a bodyless ACK
/// to the answering 2xx is clean. A later copy is still a repeat.
#[test]
fn a_retransmission_of_a_lost_arrival_is_the_first_copy_the_view_holds() {
    use crate::types::RecvDisposition::{Delivered, LossModel};
    let invite = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
    let evs = vec![
        arrival(BOB, invite.clone(), ALICE, 0, LossModel, None),
        arrival(BOB, invite.clone(), ALICE, 1, Delivered, Some(0)),
        arrival(BOB, invite, ALICE, 2, Delivered, Some(0)),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 3),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 4),
    ];
    let visible: Vec<_> =
        evs.iter().filter(|s| crate::contracts::audit_visible_event(&s.event)).cloned().collect();
    let views = bind_views(&visible);
    let repeats: Vec<bool> = views[0].msgs.iter().map(|m| m.repeat).collect();
    assert_eq!(repeats, vec![false, true, false, false], "only the second copy repeats");
    let findings = evaluate_rfc_findings(&evs);
    assert!(
        !findings.iter().any(|f| f.rule == rfc_rules::RuleId::DelayedOfferAnsweredInAck.token()),
        "{findings:?}",
    );
}

#[test]
fn a_final_leaving_the_offer_unanswered_is_flagged_at_its_wire_position() {
    assert!(!Final2xxAnswersTheOfferRule.force_advisory(), "a MUST gates");

    // Judged from the ANSWERER's bind: it takes the offer and ends the
    // round with a 200 stating no plan of its own.
    let clean = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(Final2xxAnswersTheOfferRule.check(&clean).is_empty());

    let silent = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = Final2xxAnswersTheOfferRule.check_positioned(&silent);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported on the answerer's own lane");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer");
    assert!(out[0].1.contains("audio/RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§13.2.1"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the 200 that answered nothing");
}

#[test]
fn a_reliable_provisional_answer_discharges_the_final() {
    let evs = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, reliable_resp_sdp(183, 1, "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("PRACK", "z9hG4bK-p1", 2, Some("bt"), None), ALICE, 2),
        sent(BOB, resp_sdp(200, 2, "PRACK", "z9hG4bK-p1", None), ALICE, 3),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
    ];
    assert!(Final2xxAnswersTheOfferRule.check(&evs).is_empty(), "RFC 3262 §5 answered it");
}

#[test]
fn a_silent_answerer_is_named_on_the_lane_that_took_it() {
    // The callee is no recorded bind; the caller's lane is where the
    // recording checks the round at all.
    let evs = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", None), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
    ];
    let out = Final2xxAnswersTheOfferRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "reported at the vantage that took the 200");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the answerer all the same");
}

#[test]
fn a_failure_final_and_a_cancelled_round_owe_no_answer() {
    let rejected = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(486, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(Final2xxAnswersTheOfferRule.check(&rejected).is_empty());

    let cancelled = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(180, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 1),
        recv(BOB, req_sdp("CANCEL", "z9hG4bK-i1", 1, None, None), ALICE, 2),
        sent(BOB, resp_sdp(200, 1, "CANCEL", "z9hG4bK-i1", None), ALICE, 3),
        sent(BOB, resp_sdp(487, 1, "INVITE", "z9hG4bK-i1", None), ALICE, 4),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 5),
    ];
    assert!(Final2xxAnswersTheOfferRule.check(&cancelled).is_empty());
}

#[test]
fn an_answer_re_typing_the_offered_stream_is_flagged_on_the_answerer() {
    // Judged from the ANSWERER's bind: it takes the offer and sends back a
    // stream of another type.
    let clean = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(T38_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    assert!(AnswerStreamMatchesOfferRule.check(&clean).is_empty());

    let retyped = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(T38_OFFER)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = AnswerStreamMatchesOfferRule.check_positioned(&retyped);
    assert_eq!(out.len(), 1, "one answer is one finding: {out:?}");
    assert_eq!(out[0].0, BOB);
    assert!(out[0].1.contains("m=[0]"), "{}", out[0].1);
    assert!(out[0].1.contains("image 27500 udptl"), "{}", out[0].1);
    assert!(out[0].1.contains("audio 60788 RTP/AVP"), "{}", out[0].1);
    assert!(out[0].1.contains("§6"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the answering 200");
}

#[test]
fn an_origin_that_leaves_the_session_is_advisory_and_positioned() {
    assert!(SdpOriginContinuityRule.force_advisory());
    assert_eq!(SdpOriginContinuityRule.subject(), HashSet::from([UaRole::Uac, UaRole::Uas]));
    assert!(SdpOriginContinuityRule.check(&t38_call(None, None)).is_empty());

    let out = SdpOriginContinuityRule.check_positioned(&t38_call(None, Some(STRAY_ANSWER)));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("o=bob 1 1 IN IP4 127.0.0.1"), "{}", out[0].1);
    assert!(out[0].1.contains("o=alice 424242 2 IN IP4 10.0.0.1"), "{}", out[0].1);
    assert!(out[0].1.contains("different session"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(7), "offending points at the ACK carrying the foreign origin");
}

#[test]
fn a_relay_lane_carrying_both_agents_origins_is_not_judged() {
    // A transparent proxy forwards alice's offer AND bob's answer on one
    // bind, so its sent stream interleaves o=alice and o=bob for one
    // Call-ID. That is the two agents' continuity, never the relay's.
    let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER));
    let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(AUDIO_ANSWER));
    let evs = vec![
        recv(SUT, inv.clone(), ALICE, 0),
        sent(SUT, inv, BOB, 1),
        recv(SUT, ok.clone(), BOB, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    assert!(
        SdpOriginContinuityRule.check(&evs).is_empty(),
        "{:?}",
        SdpOriginContinuityRule.check(&evs)
    );
    assert!(AnswerStreamMatchesOfferRule.check(&evs).is_empty());
    assert!(AckBodyAfterCompleteOfferAnswerRule.check(&evs).is_empty());
}

// ── the RFC 3264 offer/answer-model rules ───────────────────────────────

/// One audio stream, `sendrecv`, PT 0 bound to PCMU and PT 96 to opus.
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

/// An answer wrong four ways at once: an added stream, a rewritten `t=`,
/// video where audio was offered, and a bare port-0 rejection.
const BAD_ANSWER: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=100 200\r\n\
m=video 50000 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n\
m=audio 0 RTP/AVP\r\n";

const OFFER_2MEDIA: &str = "v=0\r\n\
o=alice 1 1 IN IP4 10.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 49172 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

const ANSWER_2MEDIA: &str = "v=0\r\n\
o=bob 2 2 IN IP4 10.0.0.2\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 50000 RTP/AVP 0\r\n\
a=sendrecv\r\n\
m=video 50002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=sendrecv\r\n";

/// INVITE(offer) → 200(answer) → ACK on the CALLER's bind: the caller sends
/// the offer and TAKES the answer, so the answerer is no recorded bind.
fn sdp_round(offer: &str, answer: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(offer)), BOB, 0),
        recv(ALICE, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(answer)), BOB, 1),
        sent(ALICE, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), BOB, 2),
    ]
}

#[test]
fn a_second_offer_over_an_unanswered_one_is_advisory_at_either_end() {
    assert!(NoNewOfferWhileOfferPendingRule.force_advisory());
    assert!(NoNewOfferWhileOfferPendingRule.check(&t38_call(None, None)).is_empty());

    let glare = vec![
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), BOB, 0),
        sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), BOB, 1),
    ];
    let out = NoNewOfferWhileOfferPendingRule.check_positioned(&glare);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "charged to the lane that sent the second offer");
    assert!(out[0].1.contains("MUST-002"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the second offer");

    // The same act at the lane that only TOOK the two offers: the offender
    // is no recorded bind, and the recording is where it is checked at all.
    let taker = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(AUDIO_OFFER)), ALICE, 0),
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(T38_OFFER)), ALICE, 1),
    ];
    let out = NoNewOfferWhileOfferPendingRule.check(&taker);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB, "reported on the lane that took it");
}

#[test]
fn a_bad_answer_is_named_at_the_offerers_lane() {
    assert!(AnswerMLineCountMatchesOfferRule
        .check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO))
        .is_empty());

    let evs = sdp_round(OFFER_1AUDIO, BAD_ANSWER);
    for (rule, must, quote) in [
        (
            Box::new(AnswerMLineCountMatchesOfferRule) as Box<dyn CrossMessageAuditRule>,
            "MUST-018",
            "video/RTP/AVP",
        ),
        (Box::new(AnswerTLineEqualsOfferRule), "MUST-019", "100 200"),
        (Box::new(AnswerMediaTypeMatchesOfferRule), "MUST-022", "m=[0]"),
        (Box::new(RejectedStreamMinimalAnswerRule), "MUST-021", "m=[1]"),
    ] {
        let out = rule.check_positioned(&evs);
        assert_eq!(out.len(), 1, "{}: {out:?}", rule.name());
        assert_eq!(out[0].0, ALICE, "{}: reported on the lane that took it", rule.name());
        assert!(out[0].1.contains(must), "{}: {}", rule.name(), out[0].1);
        assert!(out[0].1.contains(quote), "{}: {}", rule.name(), out[0].1);
        assert_eq!(out[0].2, Some(2), "{}: offending points at the answer", rule.name());
    }

    // The answerer's OWN lane names it too — it is the charged party.
    let at_answerer = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
    ];
    let out = AnswerMLineCountMatchesOfferRule.check(&at_answerer);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, BOB);
}

#[test]
fn the_direction_and_zero_port_pairings_are_advisory() {
    assert!(DirectionPairValidRule.force_advisory());
    assert!(ZeroPortPropagationRule.force_advisory());

    let held = OFFER_1AUDIO.replace("a=sendrecv", "a=inactive");
    let out = DirectionPairValidRule.check_positioned(&sdp_round(&held, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-023"), "{}", out[0].1);
    assert!(out[0].1.contains("answer 'sendrecv' invalid for offer 'inactive'"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2));

    let disabled = OFFER_1AUDIO.replace("m=audio 49170 RTP/AVP 0 96", "m=audio 0 RTP/AVP 0 96");
    assert!(DirectionPairValidRule.check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO)).is_empty());
    let out = ZeroPortPropagationRule.check_positioned(&sdp_round(&disabled, ANSWER_1AUDIO));
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("MUST-044"), "{}", out[0].1);
    assert!(out[0].1.contains("m=[0] answered port=50000"), "{}", out[0].1);
}

#[test]
fn a_shrinking_re_offer_is_charged_only_to_the_lane_that_sent_it() {
    let mut evs = sdp_round(OFFER_2MEDIA, ANSWER_2MEDIA);
    evs.push(sent(
        ALICE,
        req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)),
        BOB,
        3,
    ));
    let out = ReOfferMLineCountMonotonicRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("MUST-042"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the shrinking re-offer");

    // The lane that only TOOK that stream authored nothing: the rule judges
    // what a lane SENT, so it is charged at the sender's vantage alone.
    let taker = vec![
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_2MEDIA)), ALICE, 0),
        sent(BOB, resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(ANSWER_2MEDIA)), ALICE, 1),
        recv(BOB, req_sdp("ACK", "z9hG4bK-a1", 1, Some("bt"), None), ALICE, 2),
        recv(BOB, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(OFFER_1AUDIO)), ALICE, 3),
    ];
    assert!(ReOfferMLineCountMonotonicRule.check(&taker).is_empty());
}

#[test]
fn a_rebound_payload_type_is_named_at_either_end() {
    assert!(PayloadTypeMappingStableRule.check(&sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO)).is_empty());

    let reoffer = OFFER_1AUDIO.replace("a=rtpmap:96 opus/48000/2", "a=rtpmap:96 H264/90000");
    let mut evs = sdp_round(OFFER_1AUDIO, ANSWER_1AUDIO);
    evs.push(sent(ALICE, req_sdp("INVITE", "z9hG4bK-i3", 3, Some("bt"), Some(&reoffer)), BOB, 3));
    let out = PayloadTypeMappingStableRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE);
    assert!(out[0].1.contains("MUST-047"), "{}", out[0].1);
    assert!(
        out[0].1.contains("payload-type 96 was 'opus/48000/2' now 'H264/90000'"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(4));
}

#[test]
fn a_relay_lane_carrying_a_negotiation_is_not_judged_on_it() {
    // The relay forwards both agents' descriptions on one Call-ID and has
    // no offer/answer state of its own, so the bad answer it passes through
    // is the upstream's and is flagged on the upstream lane.
    let inv = req_sdp("INVITE", "z9hG4bK-i1", 1, None, Some(OFFER_1AUDIO));
    let ok = resp_sdp(200, 1, "INVITE", "z9hG4bK-i1", Some(BAD_ANSWER));
    let evs = vec![
        recv(SUT, inv.clone(), ALICE, 0),
        sent(SUT, inv, BOB, 1),
        recv(SUT, ok.clone(), BOB, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    let rules: Vec<Box<dyn CrossMessageAuditRule>> = vec![
        Box::new(NoNewOfferWhileOfferPendingRule),
        Box::new(AnswerMLineCountMatchesOfferRule),
        Box::new(AnswerTLineEqualsOfferRule),
        Box::new(AnswerMediaTypeMatchesOfferRule),
        Box::new(DirectionPairValidRule),
        Box::new(RejectedStreamMinimalAnswerRule),
        Box::new(ReOfferMLineCountMonotonicRule),
        Box::new(ZeroPortPropagationRule),
        Box::new(PayloadTypeMappingStableRule),
    ];
    for rule in rules {
        let out = rule.check(&evs);
        assert!(out.is_empty(), "{}: {out:?}", rule.name());
    }
}

// ── the peer seam: one message at one vantage ───────────────────────────
//
// What this adapter adds to the per-message bodies: which END of the
// offending message a finding is reported on, the `{Proxy}` narrowing, the
// advisory flag, and the wire position a peer finding could never state.

/// A request whose top Via branch carries no magic cookie.
fn legacy_branch_invite() -> Vec<u8> {
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=legacy-2543\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A sender-minted defect is charged to the lane that WROTE it, at the wire
/// position of the offending message — and a lane that merely took those
/// bytes is charged nothing.
#[test]
fn a_minted_defect_is_charged_to_its_sender_and_positioned() {
    let out = BranchPrefixRule.check_positioned(&[sent(ALICE, legacy_branch_invite(), BOB, 0)]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "the sender is charged");
    assert!(out[0].1.contains("magic cookie"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(1), "the offending wire entry");

    let taken = BranchPrefixRule.check(&[recv(SUT, legacy_branch_invite(), ALICE, 0)]);
    assert!(taken.is_empty(), "a lane that only TOOK the bytes minted nothing: {taken:?}");
}

/// A rule that judges what a lane's PEER sent IT is reported on the lane
/// that took it — the recording is the only place that peer's stream is
/// checked at all.
#[test]
fn a_peer_side_defect_is_reported_on_the_lane_that_took_it() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("CANCEL", "z9hG4bK-other", 1, None), ALICE, 1),
    ];
    let out = CancelViaBranchRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took the CANCEL");
    assert!(out[0].1.contains("matches no received INVITE"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2));

    // The same CANCEL matching the INVITE's branch discharges it.
    let clean = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 1),
    ];
    assert!(CancelViaBranchRule.check(&clean).is_empty());
}

/// A B2BUA Record-Route is judged only on a lane that declared itself a
/// proxy — the one that could be mistaken for one.
#[test]
fn the_record_route_rule_is_narrowed_to_declared_proxies() {
    let rr = format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Max-Forwards: 70\r\n\
             Record-Route: <sip:127.0.0.1:5080;callRef=7;leg=a>\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes();
    let named = |roles: HashSet<UaRole>| -> Vec<RfcFinding> {
        let evs = vec![bind_roles(SUT, roles, 0), sent(SUT, rr.clone(), BOB, 1)];
        evaluate_rfc_findings(&evs)
            .into_iter()
            .filter(|f| f.rule == NoRecordRouteFromUaRule.name())
            .collect()
    };
    let as_proxy = named(HashSet::from([UaRole::Proxy]));
    assert_eq!(as_proxy.len(), 1, "{as_proxy:?}");
    assert!(as_proxy[0].detail.contains("MUST NOT use Record-Route"), "{}", as_proxy[0].detail);
    assert!(named(HashSet::from([UaRole::Uas])).is_empty(), "a plain UAS lane is not judged");
}

/// A UAS To-tag flip on a **2xx** is SURFACED and never gates: a forking
/// B2BUA answering off a later early dialog is indistinguishable from one
/// per branch.
#[test]
fn a_uas_tag_flip_on_a_2xx_is_advisory() {
    let evs = vec![
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
        sent(SUT, resp(200, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 1),
    ];
    let f: Vec<RfcFinding> = evaluate_rfc_findings(&evs)
        .into_iter()
        .filter(|f| f.rule == TagConsistencyRule.name())
        .collect();
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].lane, SUT);
    assert!(f[0].advisory, "the tag flip is informational");
    assert!(f[0].detail.contains("To-tag mismatch"), "{}", f[0].detail);
}

/// The same flip on a NON-2xx GATES, and is reported once: a final that
/// establishes no dialog cannot be a fresh early dialog's answer, so the
/// forked-2xx waiver does not reach it.
#[test]
fn a_uas_tag_flip_on_a_non_2xx_gates() {
    let evs = vec![
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 1),
    ];
    let f: Vec<RfcFinding> = evaluate_rfc_findings(&evs)
        .into_iter()
        .filter(|f| f.rule == TagConsistencyNon2xxRule.name())
        .collect();
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].lane, SUT);
    assert!(!f[0].advisory, "a non-2xx tag flip fails the run");
    assert!(f[0].detail.contains("To-tag mismatch on 486"), "{}", f[0].detail);
}

/// A final carrying a tag one of the transaction's own provisionals showed
/// is compliant on either arm — the rule charges the FLIP, not the fork.
#[test]
fn a_final_under_a_shown_tag_charges_neither_arm() {
    let evs = vec![
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
    ];
    assert!(evaluate_rfc_findings(&evs).into_iter().all(|f| f.rule != TagConsistencyRule.name()));
}

// ── rack-without-known-invite (merged, via this adapter) ────────────────
//
// The merged rule charges the PRACK's SENDER, so the fixtures are the
// sender's own lane: the INVITE it opened and the PRACK it then emitted.

fn prack_with_rack(rack: &str) -> Vec<u8> {
    format!(
        "PRACK {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-p\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-rack@127.0.0.1\r\n\
             CSeq: 2 PRACK\r\n\
             RAck: {rack}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn rack_invite(branch: &str, cseq: u32) -> Vec<u8> {
    format!(
        "INVITE {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\n\
             From: <{A}>;tag=at\r\n\
             To: <{B}>\r\n\
             Call-ID: cid-rack@127.0.0.1\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <{A}>\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[test]
fn rack_matching_opened_invite_is_clean() {
    // RAck "1 1 INVITE" → references INVITE CSeq 1, which alice opened.
    let evs = vec![
        sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
        sent(ALICE, prack_with_rack("1 1 INVITE"), BOB, 1),
    ];
    assert!(RackWithoutKnownInviteRule.check(&evs).is_empty());
}

#[test]
fn rack_referencing_unknown_invite_is_flagged() {
    // RAck references INVITE CSeq 7 — never opened.
    let evs = vec![
        sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
        sent(ALICE, prack_with_rack("1 7 INVITE"), BOB, 1),
    ];
    let f = RackWithoutKnownInviteRule.check(&evs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, ALICE, "the PRACK's sender is charged");
    assert!(f[0].1.contains("names no INVITE this bind opened"), "{}", f[0].1);
}

/// The finding quotes the WHOLE RAck (a bare CSeq-num reads as the PRACK's
/// own CSeq) and, with one INVITE opened, names the header the sender
/// should have emitted.
#[test]
fn rack_finding_quotes_the_header_and_names_the_expected_value() {
    let evs = vec![
        sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
        sent(ALICE, prack_with_rack("12800221 101 INVITE"), BOB, 1),
    ];
    let f = RackWithoutKnownInviteRule.check(&evs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("\"RAck: 12800221 101 INVITE\""), "{}", f[0].1);
    assert!(f[0].1.contains("expected \"RAck: 12800221 1 INVITE\""), "{}", f[0].1);
    assert!(f[0].1.contains("NOT from the PRACK's"), "{}", f[0].1);
}

/// Several INVITEs opened (initial + re-INVITE) leave no single correct
/// value, so the finding lists what was opened and offers no repair.
#[test]
fn rack_finding_offers_no_expected_value_when_ambiguous() {
    let evs = vec![
        sent(ALICE, rack_invite("z9hG4bK-i", 1), BOB, 0),
        sent(ALICE, rack_invite("z9hG4bK-i2", 5), BOB, 1),
        sent(ALICE, prack_with_rack("1 101 INVITE"), BOB, 2),
    ];
    let f = RackWithoutKnownInviteRule.check(&evs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("opened INVITE CSeq: 1, 5"), "{}", f[0].1);
    assert!(!f[0].1.contains("expected"), "{}", f[0].1);
}

/// A vantage that only TOOK the PRACK charges nobody it can see: the sender
/// is a foreign endpoint, and the live policy charges the vantage.
#[test]
fn rack_on_a_receiving_bind_charges_nothing() {
    let evs = vec![
        recv(SUT, rack_invite("z9hG4bK-i", 1), ALICE, 0),
        recv(SUT, prack_with_rack("1 7 INVITE"), ALICE, 1),
    ];
    assert!(RackWithoutKnownInviteRule.check(&evs).is_empty());
}

// ── the §17 / §13.3.1.4 / RFC 3262 §3 rung family ────────────────────────

/// The same datagram with its From and To rows swapped: every header the
/// first copy carried, and not the same response.
fn recomposed(raw: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8(raw).unwrap();
    let mut rows: Vec<&str> = text.split("\r\n").collect();
    rows.swap(2, 3);
    rows.join("\r\n").into_bytes()
}

/// The violation, at its wire position: the SUT answered 200 and its
/// repeat was re-composed rather than repeated.
#[test]
fn a_re_composed_2xx_rung_is_charged_to_the_lane_that_sent_it() {
    let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, ok.clone(), ALICE, 1),
        sent(SUT, recomposed(ok), ALICE, 2),
    ];
    let out = RungByteIdenticalRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that re-composed its rung");
    assert_eq!(out[0].3.as_deref(), Some(SUT));
    let detail = &out[0].1;
    assert!(detail.contains("2xx final") && detail.contains("200 (INVITE)"), "{detail}");
    assert!(detail.contains("z9hG4bK-i") && detail.contains("cid-1@127.0.0.1"), "{detail}");
    assert!(detail.contains("copy 1 of 2") && detail.contains("in the head at byte"), "{detail}");
    assert!(detail.contains("first `From: <sip:alice@127.0.0.1>;tag=at`"), "{detail}");
    assert!(detail.contains("this copy `To: <sip:bob@127.0.0.1>;tag=bt`"), "{detail}");
    assert!(detail.contains("§13.3.1.4"), "{detail}");
    assert_eq!(out[0].2, Some(3), "offending points at the divergent copy");
    assert!(!RungByteIdenticalRule.force_advisory(), "a divergent rung gates");
}

/// The LB proxy's own Record-Route cookie may differ between two copies of
/// one forwarded INVITE: the lane key IS the emitter's `ip:port`, spelled
/// as the proxy's Record-Route URI spells it, so the rule masks that row —
/// and no other.
#[test]
fn a_re_stamped_own_record_route_cookie_is_not_charged_but_a_foreign_row_is() {
    const WORKER: &str = "127.0.0.1:5091";
    let forwarded = |cookie: &str, foreign: &str| {
        format!(
            "INVITE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-p;rport\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i;received=127.0.0.1;rport=5060\r\n\
                 Record-Route: <sip:127.0.0.1:5080;outbound;lr>\r\n\
                 Record-Route: <sip:127.0.0.1:5080;w_pri=b1;w_bak={cookie};lr>\r\n\
                 Record-Route: <sip:10.0.0.7:5060;{foreign}>\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Max-Forwards: 69\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, forwarded("b2", "lr"), WORKER, 1),
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
        sent(SUT, forwarded("\"\"", "lr"), WORKER, 3),
    ];
    assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());

    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, forwarded("b2", "lr"), WORKER, 1),
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
        sent(SUT, forwarded("\"\"", "x=1;lr"), WORKER, 3),
    ];
    let out = RungByteIdenticalRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    let detail = &out[0].1;
    assert!(detail.contains("INVITE request"), "{detail}");
    assert!(detail.contains("first `Record-Route: <sip:10.0.0.7:5060;lr>`"), "{detail}");
    assert!(detail.contains("this copy `Record-Route: <sip:10.0.0.7:5060;x=1;lr>`"), "{detail}");
}

/// The obliged behaviour: the same bytes again is nothing to report, and
/// the projection's repeat mark does not change that.
#[test]
fn a_byte_identical_2xx_rung_is_not_charged() {
    let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, ok.clone(), ALICE, 1),
        sent(SUT, ok.clone(), ALICE, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
}

/// A B2BUA relaying two forks' 2xx sends two messages under one branch and
/// CSeq; the tag tells them apart, and neither is a rung of the other.
#[test]
fn another_forks_2xx_is_not_a_rung() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 2),
    ];
    assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
}

/// The far end's re-composed rung is not this bind's: the rule charges the
/// emitter, and the live policy surfaces a finding only where the charged
/// party IS the vantage.
#[test]
fn a_re_composed_rung_the_peer_sent_is_not_charged_to_this_bind() {
    let bye = req("BYE", "z9hG4bK-b", 2, Some("bt"));
    let evs = vec![recv(SUT, bye.clone(), ALICE, 0), recv(SUT, recomposed(bye), ALICE, 1)];
    assert!(RungByteIdenticalRule.check_positioned(&evs).is_empty());
}

/// A reliable provisional the SUT re-sent under the same `RSeq` with other
/// bytes is charged under RFC 3262 §3; the next `RSeq` is another message.
#[test]
fn a_re_composed_reliable_provisional_rung_is_charged() {
    let reliable = |rseq: u32| {
        format!(
            "SIP/2.0 183 Session Progress\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Require: 100rel\r\n\
                 RSeq: {rseq}\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, reliable(1), ALICE, 1),
        sent(SUT, recomposed(reliable(1)), ALICE, 2),
        sent(SUT, reliable(2), ALICE, 3),
    ];
    let out = RungByteIdenticalRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("reliable provisional 183 RSeq 1 (INVITE)"), "{}", out[0].1);
    assert!(out[0].1.contains("RFC 3262 §3"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3));
}
