//! The live surfacing of the re-INVITE family rules.

use super::*;

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
