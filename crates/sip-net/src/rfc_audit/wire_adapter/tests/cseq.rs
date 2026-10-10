//! The live surfacing of the CSeq family rules.

use super::*;

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
