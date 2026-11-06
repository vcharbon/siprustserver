//! Where `MidDialogWireDestination` has nothing to judge: no confirmed dialog
//! yet, or a target whose scheme names no host to resolve (RFC 3263 §4).

use crate::verdict::Decision;

use super::super::MidDialogWireDestination;
use super::{eval, invite, req, rsp, ALICE, A_URI, BOB, B_URI};

/// An unconfirmed dialog may still be going through a configured outbound
/// proxy (§8.1.2): no occasion before the peer has a tag.
#[test]
fn an_unconfirmed_dialog_opens_no_occasion() {
    let msgs = [invite(1_000, "z9hG4bK-i", "")];
    assert!(eval(&MidDialogWireDestination, &msgs).is_empty());
}

/// A Request-URI of a scheme that names no host resolves to no destination,
/// so the wire is not judged against one.
#[test]
fn a_request_uri_naming_no_host_is_undecidable() {
    let bye = req(
        3_000,
        ALICE,
        BOB,
        "BYE",
        "urn:service:sos",
        "z9hG4bK-b",
        2,
        A_URI,
        B_URI,
        Some("bt"),
        "",
    );
    let msgs = [invite(1_000, "z9hG4bK-i", ""), rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""), bye];
    let all = eval(&MidDialogWireDestination, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Undecidable(_)), "{:?}", all[0].decision);
}

/// RFC 3263 §4: a Request-URI's `maddr` is its target, at the URI's port, whatever
/// the host says.
#[test]
fn a_request_uris_maddr_is_the_destination_it_names() {
    let bye = |uri: &str| {
        req(3_000, ALICE, BOB, "BYE", uri, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), "")
    };
    let judged = |uri: &str| {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            bye(uri),
        ];
        let all = eval(&MidDialogWireDestination, &msgs);
        assert_eq!(all.len(), 1, "{all:?}");
        all[0].decision.clone()
    };
    assert!(matches!(judged("sip:bob@name.invalid:5070;maddr=127.0.0.1"), Decision::Compliant));
    assert!(matches!(judged("sip:bob@127.0.0.1:5070;maddr=192.0.2.9"), Decision::Violated(_)));
}
