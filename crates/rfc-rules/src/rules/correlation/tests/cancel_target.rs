//! A CANCEL against the INVITE it cancels.

use super::*;

// ── cancel-request-uri / cancel-via-branch ──────────────────────────────

#[test]
fn a_cancel_matching_its_invite_is_compliant() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
    ];
    assert!(charged(run(&CancelRequestUri, &msgs), A)
        .iter()
        .all(|x| matches!(x.decision, Decision::Compliant)));
    assert!(charged(run(&CancelViaBranch, &msgs), A)
        .iter()
        .all(|x| matches!(x.decision, Decision::Compliant)));
}

#[test]
fn a_cancel_naming_another_uri_is_violated() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
    ];
    let f = charged(run(&CancelRequestUri, &msgs), A);
    let Decision::Violated(Evidence::CancelUriDiverged { cancel_uri, invite_uri, .. }) =
        &f[0].decision
    else {
        panic!("cancel-uri evidence: {:?}", f[0].decision)
    };
    assert_eq!((cancel_uri.as_str(), invite_uri.as_str()), ("sip:eve@h", "sip:bob@h"));
}

/// A CANCEL on an unmatched branch is the BRANCH rule's finding; the URI
/// rule has no INVITE to compare against and states nothing.
#[test]
fn a_cancel_on_a_foreign_branch_is_the_branch_rules_finding() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(2_000, A, B, "CANCEL", "sip:eve@h", 1, "z9hG4bK-other", "sip:alice@h", "at", None, 1),
    ];
    assert!(run(&CancelRequestUri, &msgs).is_empty());
    let f = charged(run(&CancelViaBranch, &msgs), A);
    let Decision::Violated(Evidence::CancelBranchUnmatched {
        cancel_branch, invite_branches, ..
    }) = &f[0].decision
    else {
        panic!("cancel-branch evidence: {:?}", f[0].decision)
    };
    assert_eq!(cancel_branch, "z9hG4bK-other");
    assert_eq!(invite_branches.as_slice(), ["z9hG4bK-i"]);
}

/// RFC 3261 §9.1 on the UAC side: a CANCEL sent while its INVITE rings must
/// carry that INVITE's top Via branch. One sent on another branch matches no
/// server transaction, and its sender is charged, whatever the UAS answers
/// next; the same CANCEL on the INVITE's branch is compliant.
#[test]
fn a_cancel_off_its_ringing_invite_s_branch_is_charged_to_its_sender() {
    let ringing = |cancel_branch| {
        vec![
            req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-b1", "sip:alice@h", "at", None, 1),
            resp(2_000, B, A, 180, 1, "INVITE", "z9hG4bK-b1", "at", Some("bt"), 1),
            req(3_000, A, B, "CANCEL", "sip:bob@h", 1, cancel_branch, "sip:alice@h", "at", None, 1),
            resp(4_000, B, A, 200, 1, "INVITE", "z9hG4bK-b1", "at", Some("bt"), 1),
        ]
    };
    let f = run(&CancelViaBranch, &ringing("z9hG4bK-b2"));
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!((f[0].emitter.as_str(), f[0].anchor), (A, 2), "the CANCEL's sender is charged");
    assert!(f[0].violated(), "{:?}", f[0].decision);

    let on_its_branch = run(&CancelViaBranch, &ringing("z9hG4bK-b1"));
    assert_eq!(on_its_branch.len(), 1, "{on_its_branch:?}");
    assert!(matches!(on_its_branch[0].decision, Decision::Compliant), "{on_its_branch:?}");
}

/// A re-INVITE opens a second transaction, and a CANCEL matching EITHER
/// branch is fine.
#[test]
fn a_cancel_of_a_re_invite_branch_is_compliant() {
    let msgs = vec![
        req(1_000, A, B, "INVITE", "sip:bob@h", 1, "z9hG4bK-i", "sip:alice@h", "at", None, 1),
        req(
            2_000,
            A,
            B,
            "INVITE",
            "sip:bob@h",
            2,
            "z9hG4bK-i2",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
        req(
            3_000,
            A,
            B,
            "CANCEL",
            "sip:bob@h",
            2,
            "z9hG4bK-i2",
            "sip:alice@h",
            "at",
            Some("bt"),
            1,
        ),
    ];
    let f = charged(run(&CancelViaBranch, &msgs), A);
    assert!(f.iter().all(|x| matches!(x.decision, Decision::Compliant)), "{f:?}");
}

/// Never having seen an INVITE, nothing can say the CANCEL is orphaned.
#[test]
fn a_cancel_without_any_invite_is_no_occasion() {
    let msgs =
        vec![req(1_000, A, B, "CANCEL", "sip:bob@h", 1, "z9hG4bK-c", "sip:alice@h", "at", None, 1)];
    assert!(run(&CancelViaBranch, &msgs).is_empty());
}
