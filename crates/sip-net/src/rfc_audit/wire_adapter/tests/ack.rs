//! The live surfacing of the ACK family rules.

use super::*;

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
