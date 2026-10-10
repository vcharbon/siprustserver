//! The live surfacing of the well-formedness family rules.

use super::*;

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
