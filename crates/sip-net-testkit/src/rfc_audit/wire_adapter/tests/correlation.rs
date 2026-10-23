//! The live surfacing of the correlation family rules.

use super::*;

/// §12.2.1.1 GATES: a request the caller takes long after a forking
/// callee's answer, under the tag of the branch that only rang, is flagged
/// at the caller's lane and charged to its sender.
#[test]
fn a_request_under_a_rung_branch_tag_after_the_answer_gates() {
    assert!(!live(RuleId::InDialogFromTag).force_advisory(), "a MUST gates");
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
    assert!(live(RuleId::InDialogFromTag).check(&call("b2")).is_empty());
    let out = live(RuleId::InDialogFromTag).check_positioned(&call("b1"));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, ALICE, "reported at the caller, whose dialog it fails to name");
    assert_eq!(out[0].3, Some(BOB.to_string()), "charged to the request's sender");
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
    let out = live(RuleId::CancelViaBranch).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took the CANCEL");
    assert!(out[0].1.contains("matches no received INVITE"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2));

    // The same CANCEL matching the INVITE's branch discharges it.
    let clean = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 1),
    ];
    assert!(live(RuleId::CancelViaBranch).check(&clean).is_empty());
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
        .filter(|f| f.rule == live_arm(RuleId::TagConsistency, true).name())
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
        .filter(|f| f.rule == live_arm(RuleId::TagConsistency, false).name())
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
    assert!(evaluate_rfc_findings(&evs)
        .into_iter()
        .all(|f| f.rule != live_arm(RuleId::TagConsistency, true).name()));
}
