//! The live surfacing of the final-response family rules.

use super::*;

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
    let out = live(RuleId::SingleFinalPerServerTxn).check_positioned(&evs);
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
    let out = live(RuleId::SingleFinalPerServerTxn).check_positioned(&evs);
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
    assert!(live(RuleId::SingleFinalPerServerTxn).check_positioned(&evs).is_empty());
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
        live(RuleId::SingleFinalPerServerTxn).check_positioned(&evs).is_empty(),
        "a 1xx is not a final — this rule does not judge it"
    );
    assert_eq!(
        live(RuleId::No1xxAfterFinal).check(&evs).len(),
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
    assert!(live(RuleId::SingleFinalPerServerTxn).check_positioned(&evs).is_empty());
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
    let out = live(RuleId::SingleFinalPerServerTxn).check(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
}

/// §13.3.1.1 at the UAS lane: a NEW provisional after the final fires and
/// points at itself; a relay lane forwarding its upstream's is not judged.
#[test]
fn a_late_provisional_fires_at_the_uas_lane_and_not_on_a_relay() {
    assert_eq!(live(RuleId::No1xxAfterFinal).subject(), HashSet::from([UaRole::Uas]));
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
        sent(SUT, resp(181, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 4),
    ];
    let out = live(RuleId::No1xxAfterFinal).check_positioned(&evs);
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
    assert!(live(RuleId::No1xxAfterFinal).check_positioned(&retransmits).is_empty());

    // A lane that forwards both directions of one Call-ID relays what the
    // upstream produced; the late 1xx is flagged on the upstream lane.
    let relayed = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 2),
        recv(SUT, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 3),
        sent(SUT, resp(183, 1, "INVITE", "bt", "z9hG4bK-a"), ALICE, 4),
    ];
    assert!(live(RuleId::No1xxAfterFinal).check_positioned(&relayed).is_empty());
}

/// A row reported off its sender's lane is dispatched on the SENDER's roles:
/// a sender that is no recorded bind declared none, so every role-scoped row
/// judges it, whatever roles the taker declared.
#[test]
fn an_unrecorded_senders_second_final_is_dispatched_on_its_own_roles() {
    let evs = vec![
        bind_roles(ALICE, HashSet::from([UaRole::Uac]), 0),
        sent(ALICE, req("INVITE", "z9hG4bK-i", 1, None), SUT, 1),
        recv(ALICE, resp(487, 1, "INVITE", "bt", "z9hG4bK-i"), SUT, 2),
        sent(ALICE, req("ACK", "z9hG4bK-i", 1, Some("bt")), SUT, 3),
        recv(ALICE, resp(480, 1, "INVITE", "bt", "z9hG4bK-i"), SUT, 4),
    ];
    let found: Vec<RfcFinding> = evaluate_rfc_findings(&evs)
        .into_iter()
        .filter(|f| f.rule == RuleId::SingleFinalPerServerTxn.token())
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lane, ALICE, "reported where the finals were taken");
    assert_eq!(found[0].charged.as_deref(), Some(SUT));
}
