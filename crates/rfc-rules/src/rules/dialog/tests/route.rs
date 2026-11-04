//! `MidDialogRoute` (RFC 3261 §12.2.1.1 / §16.12): the route set an in-dialog
//! request owes, loose or strict, and what a record-routing proxy owes.

use crate::verdict::{Decision, Evidence};

use super::super::MidDialogRoute;
use super::{eval, hits, invite, req, rsp, ALICE, A_URI, BOB, B_URI};

/// The UAC's route set is the 200's Record-Route stack REVERSED: two rows
/// `p1, p2` owe Route rows `p2, p1`.
#[test]
fn a_replayed_loose_route_set_is_clean() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(
            2_000,
            200,
            1,
            "INVITE",
            "z9hG4bK-i",
            "Record-Route: <sip:p1@127.0.0.1;lr>\r\nRecord-Route: <sip:p2@127.0.0.1;lr>\r\n",
        ),
        req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            B_URI,
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "Route: <sip:p2@127.0.0.1;lr>\r\nRoute: <sip:p1@127.0.0.1;lr>\r\n",
        ),
    ];
    let all = eval(&MidDialogRoute, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// Dropping the Route rows on a dialog with a non-empty set is the defect.
#[test]
fn an_omitted_route_set_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
        req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
    ];
    let f = hits(&MidDialogRoute, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::MidDialogRouteDiverged {
        dialog_route_set,
        sent_routes,
        loose_first_route,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert!(loose_first_route);
    assert_eq!(dialog_route_set.len(), 1);
    assert!(sent_routes.is_empty());
}

/// A proxy legitimately rewrites its Record-Route URI PARAMETERS per
/// direction; only the routing-significant `host:port` is compared, so a
/// re-parameterised hop is clean and a re-AIMED one is not.
#[test]
fn a_reparameterised_hop_is_clean_and_a_reaimed_one_is_not() {
    let dialog = |route: &str| {
        [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(
                2_000,
                200,
                1,
                "INVITE",
                "z9hG4bK-i",
                "Record-Route: <sip:p1@127.0.0.1;lr;e=abc>\r\n",
            ),
            req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), route),
        ]
    };
    assert!(
        hits(&MidDialogRoute, &dialog("Route: <sip:p1@127.0.0.1;lr;outbound>\r\n")).is_empty(),
        "per-direction parameters are not a route change",
    );
    let f = hits(&MidDialogRoute, &dialog("Route: <sip:p9@127.0.0.2;lr;e=abc>\r\n"));
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::MidDialogRouteDiverged { first_bad_hop, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*first_bad_hop, Some(0));
}

/// A record-routing proxy's own view: the rows it forwarded on the INVITE are
/// its own hop and those upstream, so its BYE toward the UA owes Route rows
/// only for the hops recorded beyond it.
#[test]
fn a_record_routing_proxy_owes_only_the_hops_recorded_beyond_it() {
    let own = "Record-Route: <sip:p1@127.0.0.1;outbound;lr>\r\n";
    let dialog = |stack: &str, route: &str| {
        [
            invite(1_000, "z9hG4bK-i", own),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", stack),
            req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), route),
        ]
    };
    assert!(eval(&MidDialogRoute, &dialog(own, "")).is_empty(), "no hop recorded beyond it");
    let beyond = format!("Record-Route: <sip:p2@127.0.0.1;lr>\r\n{own}");
    let routed = dialog(&beyond, "Route: <sip:p2@127.0.0.1;lr>\r\n");
    assert!(hits(&MidDialogRoute, &routed).is_empty(), "the downstream hop reproduced");
    assert_eq!(hits(&MidDialogRoute, &dialog(&beyond, "")).len(), 1, "the downstream hop dropped");
}

/// A STRICT first route (§16.12) rides the Request-URI, and the real target
/// is legally appended to the Route tail.
#[test]
fn a_strict_route_set_rides_the_request_uri() {
    let dialog = |r_uri: &str, route: &str| {
        [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(
                2_000,
                200,
                1,
                "INVITE",
                "z9hG4bK-i",
                "Record-Route: <sip:p2@127.0.0.1>\r\nRecord-Route: <sip:p1@127.0.0.1>\r\n",
            ),
            req(3_000, ALICE, BOB, "BYE", r_uri, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), route),
        ]
    };
    // Reversed set = [p1, p2]: p1 becomes the Request-URI, p2 + the target
    // ride Route.
    assert!(
        hits(
            &MidDialogRoute,
            &dialog(
                "sip:p1@127.0.0.1",
                "Route: <sip:p2@127.0.0.1>\r\nRoute: <sip:bob@127.0.0.1>\r\n"
            )
        )
        .is_empty(),
        "the target appended to the tail is §16.12's own form",
    );
    let f = hits(
        &MidDialogRoute,
        &dialog(B_URI, "Route: <sip:p2@127.0.0.1>\r\nRoute: <sip:bob@127.0.0.1>\r\n"),
    );
    assert_eq!(f.len(), 1, "a Request-URI that is not the first strict route: {f:?}");
    let Decision::Violated(Evidence::MidDialogRouteDiverged {
        loose_first_route, request_uri, ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert!(!loose_first_route);
    assert_eq!(request_uri.as_str(), B_URI);
}

/// A Route row no reader accepts names no path: the routing rule states
/// nothing, and the grammar rules own the row.
#[test]
fn an_unreadable_route_row_is_undecidable() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
        req(
            3_000,
            ALICE,
            BOB,
            "BYE",
            B_URI,
            "z9hG4bK-b",
            2,
            A_URI,
            B_URI,
            Some("bt"),
            "Route: <sip:p1@127.0.0.1;lr\r\n",
        ),
    ];
    let f = eval(&MidDialogRoute, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(
        matches!(f[0].decision, Decision::Undecidable("a Route row no reader accepts")),
        "{:?}",
        f[0].decision
    );
}

/// An empty route set asks for nothing, and ACK/CANCEL take the INVITE's
/// own path — no occasion in either case.
#[test]
fn an_empty_route_set_and_the_hop_by_hop_methods_open_no_occasion() {
    let no_set = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        req(3_000, ALICE, BOB, "BYE", B_URI, "z9hG4bK-b", 2, A_URI, B_URI, Some("bt"), ""),
    ];
    assert!(eval(&MidDialogRoute, &no_set).is_empty());

    let acked = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "Record-Route: <sip:p1@127.0.0.1;lr>\r\n"),
        req(3_000, ALICE, BOB, "ACK", B_URI, "z9hG4bK-a", 1, A_URI, B_URI, Some("bt"), ""),
    ];
    assert!(eval(&MidDialogRoute, &acked).is_empty());
}
