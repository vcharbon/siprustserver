//! The live surfacing of the dialog family rules.

use super::*;

/// The establishing exchange every §12 test opens with: the SUT's INVITE
/// and the 200 that confirms the dialog, with `extra` on the 200.
fn established(extra: &str) -> Vec<Stamped<SignalingNetworkEvent>> {
    vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", extra), BOB, 1),
    ]
}

/// §12.2.1.1 at the EMITTER: the lane that rewrote the dialog's From URI
/// mid-dialog is the lane charged, and the position pins the BYE.
#[test]
fn a_rewritten_mid_dialog_uri_is_reported_on_the_lane_that_sent_it() {
    let mut evs = established("");
    evs.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, "sip:eve@127.0.0.1", B, ""), BOB, 2));
    let out = MidDialogUriRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that sent the request");
    assert!(
        out[0].1.contains("From URI") && out[0].1.contains("sip:eve@127.0.0.1"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(3), "offending points at the BYE");

    let mut clean = established("");
    clean.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
    assert!(MidDialogUriRule.check_positioned(&clean).is_empty());
}

/// A relay carries the originator's From/To through unchanged: per-UA URI
/// stability is not its invariant, so a forwarding lane is not judged.
#[test]
fn a_relay_lane_carrying_a_foreign_uri_is_not_judged() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-b"), BOB, 1),
        recv(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-b", ""), BOB, 2),
        sent(SUT, in_dialog("BYE", B, "z9hG4bK-y", 2, "sip:eve@127.0.0.1", B, ""), BOB, 3),
    ];
    assert!(MidDialogUriRule.check_positioned(&evs).is_empty());
}

/// §12.2.1.1 route reproduction at the emitter: the 200's Record-Route
/// stack reversed is what the BYE owes.
#[test]
fn a_dropped_mid_dialog_route_set_is_reported_on_the_lane_that_sent_it() {
    let mut evs = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
    evs.push(sent(SUT, in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, ""), BOB, 2));
    let out = MidDialogRouteRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("omits Route header"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the BYE");

    let mut replayed = established("Record-Route: <sip:p1@127.0.0.1;lr>\r\n");
    replayed.push(sent(
        SUT,
        in_dialog("BYE", B, "z9hG4bK-b", 2, A, B, "Route: <sip:p1@127.0.0.1;lr>\r\n"),
        BOB,
        2,
    ));
    assert!(MidDialogRouteRule.check_positioned(&replayed).is_empty());
}

/// §8.1.2 + RFC 3263 §4 at the emitter: the bytes went past the
/// destination the request's own Request-URI names.
#[test]
fn bytes_sent_past_the_derived_destination_are_reported_at_the_emitter() {
    let mut evs = established("");
    evs.push(sent(
        SUT,
        in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""),
        "127.0.0.1:9999",
        2,
    ));
    let out = MidDialogWireDestinationRule.check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("wire-sent to 127.0.0.1:9999"), "{}", out[0].1);

    let mut clean = established("");
    clean.push(sent(
        SUT,
        in_dialog("BYE", "sip:bob@127.0.0.1:5070", "z9hG4bK-b", 2, A, B, ""),
        BOB,
        2,
    ));
    assert!(MidDialogWireDestinationRule.check_positioned(&clean).is_empty());
}

/// §12.1.1 at the TAKER: the lane that TOOK the 100 reports it, and the far
/// endpoint that leaked the Record-Route is the one the text describes.
#[test]
fn record_route_on_a_100_is_reported_at_the_taker() {
    let evs = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(SUT, resp(100, 1, "INVITE", "", "z9hG4bK-i"), BOB, 1),
    ];
    assert!(RecordRoutePlacementRule.check_positioned(&evs).is_empty(), "a bare 100 is clean");

    let leaked = vec![
        sent(SUT, in_dialog_less_to_tag("z9hG4bK-i"), BOB, 0),
        recv(
            SUT,
            format!(
                "SIP/2.0 100 Trying\r\n\
                     Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-i\r\n\
                     From: <{A}>;tag=at\r\n\
                     To: <{B}>\r\n\
                     Call-ID: cid-1@127.0.0.1\r\n\
                     CSeq: 1 INVITE\r\n\
                     Record-Route: <sip:p1@127.0.0.1;lr>\r\n\
                     Content-Length: 0\r\n\r\n"
            )
            .into_bytes(),
            BOB,
            1,
        ),
    ];
    let out = RecordRoutePlacementRule.check_positioned(&leaked);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took it");
    assert!(out[0].1.contains("100 Trying carries Record-Route"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the 100");
}

/// §12.2.2 at the taker: a BYE naming a peer tag this lane never confirmed,
/// served instead of refused 481.
#[test]
fn a_request_for_an_unknown_dialog_is_flagged_on_the_lane_that_served_it() {
    /// A BYE alice sent the SUT, with a caller-chosen From tag.
    fn bye(from_tag: &str) -> Vec<u8> {
        format!(
            "BYE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-b\r\n\
                 From: <{A}>;tag={from_tag}\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 2 BYE\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }
    let confirmed = |from_tag: &str, bye_status: u16| {
        vec![
            recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
            sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
            recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
            recv(SUT, bye(from_tag), ALICE, 3),
            sent(SUT, resp(bye_status, 2, "BYE", "bt", "z9hG4bK-b"), ALICE, 4),
        ]
    };
    assert!(
        UnknownDialog481Rule.check_positioned(&confirmed("at", 200)).is_empty(),
        "the dialog the 200 confirmed is known"
    );
    assert!(
        UnknownDialog481Rule.check_positioned(&confirmed("zz", 481)).is_empty(),
        "the 481 discharges it"
    );
    let out = UnknownDialog481Rule.check_positioned(&confirmed("zz", 200));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that owed the 481 is charged");
    assert!(out[0].1.contains("481") && out[0].1.contains("answered 200"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the BYE it served");
}

/// §15 at the sending lane: a BYE with no peer tag is off-dialog, and a
/// callee that BYEs before it has accepted is the early-dialog shape.
#[test]
fn a_bye_off_dialog_and_an_early_callee_bye_both_surface() {
    let off = vec![sent(ALICE, req("BYE", "z9hG4bK-b", 2, None), BOB, 0)];
    let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&off);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, ALICE);
    assert!(f[0].1.contains("outside any dialog"), "{}", f[0].1);

    let early = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(180, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 2),
    ];
    let f = NoByeOutsideOrEarlyDialogRule.check_positioned(&early);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, BOB, "the callee is charged");
    assert!(f[0].1.contains("early dialog"), "{}", f[0].1);

    // Accepted first: the same BYE is the ordinary teardown.
    let accepted = vec![
        recv(BOB, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(BOB, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(BOB, req("ACK", "z9hG4bK-a", 1, Some("bt")), ALICE, 2),
        sent(BOB, from_callee("BYE", "z9hG4bK-b", 2), ALICE, 3),
    ];
    assert!(NoByeOutsideOrEarlyDialogRule.check_positioned(&accepted).is_empty());
}
