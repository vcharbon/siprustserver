//! The §3 negotiation of reliable provisionals.

use super::*;

/// An in-dialog UPDATE the UAS takes (RFC 3311) — a non-INVITE request, so
/// no provisional it draws may be reliable.
fn update_on(at_us: u64, extra: &str) -> Msg {
    let head = format!(
        "UPDATE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
    );
    let mut m =
        msg(at_us, UAC, UAS, Kind::Request { method: "UPDATE".to_string() }, 2, Some("tb"), head);
    m.cseq_method = "UPDATE".to_string();
    m.via_branch = Some(UPDATE_BRANCH.to_string());
    m
}

/// The UAS's response on that UPDATE transaction.
fn update_resp(at_us: u64, status: u16, extra: &str) -> Msg {
    let head = format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP h;branch={UPDATE_BRANCH}\r\n\
             From: <sip:a@h>;tag=fa\r\nTo: <sip:b@h>;tag=tb\r\nCSeq: 2 UPDATE\r\n{extra}\
             Content-Length: 0\r\n\r\n"
    );
    let mut m = msg(at_us, UAS, UAC, Kind::Response { status }, 2, Some("tb"), head);
    m.cseq_method = "UPDATE".to_string();
    m.via_branch = Some(UPDATE_BRANCH.to_string());
    m
}

/// `require-reliable-1xx-on-require`: the INVITE demanded the extension, so
/// a plain 18x is neither half of §3's disjunction — and the 420 rejecting
/// the extension discharges the transaction whenever on it it goes out.
#[test]
fn a_plain_18x_against_require_100rel_charges_the_uas() {
    let msgs = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, "", false),
    ];
    let f = decide(&RequireReliable1xxOnRequire, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].emitter, UAS);
    assert_eq!(f[0].anchor, 1, "the occasion rests on the provisional");
    let Decision::Violated(Evidence::Unreliable1xx { status, invite_msg, .. }) = &f[0].decision
    else {
        panic!("unreliable-1xx evidence: {:?}", f[0].decision)
    };
    assert_eq!((*status, *invite_msg), (180, 0));

    let reliable = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(
        decide(&RequireReliable1xxOnRequire, &reliable)[0].decision,
        Decision::Compliant
    ));

    let rejected = vec![
        invite_on(1_000_000, "Require: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, "", false),
        inv_resp(1_200_000, 420, "Unsupported: 100rel\r\n", false),
    ];
    assert!(
        decide(&RequireReliable1xxOnRequire, &rejected).iter().all(|f| !f.violated()),
        "the 420 discharges the transaction",
    );
}

/// `reliable-needs-client-opt-in`: `Supported` alone licenses the PRACK
/// machinery; an INVITE this vantage never carried witnesses no
/// negotiation and settles nothing.
#[test]
fn a_reliable_1xx_without_client_opt_in_charges_the_uas() {
    let msgs = vec![
        invite_on(1_000_000, "", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    let f = decide(&ReliableNeedsClientOptIn, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].violated(), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, UAS);

    let opted = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(decide(&ReliableNeedsClientOptIn, &opted)[0].decision, Decision::Compliant));

    let orphan = vec![inv_resp(1_100_000, 180, &reliable_rows(1), false)];
    assert!(matches!(
        decide(&ReliableNeedsClientOptIn, &orphan)[0].decision,
        Decision::Undecidable(_)
    ));
}

/// `no-reliable-1xx-on-in-dialog`: the METHOD of the request it answers is
/// what makes the provisional an offence (RFC 3262 §3 — "any method but
/// INVITE"), so an UPDATE is charged and a re-INVITE is not.
#[test]
fn a_reliable_1xx_to_a_non_invite_request_charges_the_uas() {
    let msgs = vec![
        update_on(1_000_000, "Supported: 100rel\r\n"),
        update_resp(1_100_000, 183, &reliable_rows(1)),
    ];
    let f = decide(&NoReliable1xxOnInDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::InDialogReliable1xx { method, request_msg, .. }) =
        &f[0].decision
    else {
        panic!("non-INVITE evidence: {:?}", f[0].decision)
    };
    assert_eq!((method.as_str(), *request_msg), ("UPDATE", 0));

    let initial = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", None, false),
        inv_resp(1_100_000, 180, &reliable_rows(1), false),
    ];
    assert!(matches!(decide(&NoReliable1xxOnInDialog, &initial)[0].decision, Decision::Compliant));
}

/// A re-INVITE is an INVITE: RFC 3262 §3's To-tag prohibition is written
/// for a proxy ("unlike a UAS"), and RFC 6141 §4.6 has a UAS answer a
/// target-refresh re-INVITE with a reliable provisional.
#[test]
fn a_reliable_1xx_to_a_re_invite_is_compliant() {
    let msgs = vec![
        invite_on(1_000_000, "Supported: 100rel\r\n", Some("tb"), false),
        inv_resp(1_100_000, 183, &reliable_rows(1), false),
    ];
    let f = decide(&NoReliable1xxOnInDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}
