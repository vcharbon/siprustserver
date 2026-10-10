//! The live surfacing of the RFC 3262 family rules.

use super::*;

fn body_rows(sdp: bool) -> String {
    match sdp {
        true => format!("Content-Type: application/sdp\r\nContent-Length: {}\r\n", SDP.len()),
        false => "Content-Length: 0\r\n".to_string(),
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
