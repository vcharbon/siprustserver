//! The live surfacing of the CANCEL family rules.

use super::*;

/// A request the SUT sends on its own b-leg — the leg it drives as UAC,
/// which is where a CANCEL of its own making rides.
fn b_leg_req(method: &str, branch: &str, ttag: Option<&str>) -> Vec<u8> {
    let to = match ttag {
        Some(t) => format!("<{B}>;tag={t}"),
        None => format!("<{B}>"),
    };
    format!(
        "{method} {B} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: {to}\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The far end's answer on that b-leg.
fn b_leg_resp(status: u16, branch: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} Response\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
             From: <{A}>;tag=bft\r\n\
             To: <{B}>;tag=bt\r\n\
             Call-ID: cid-b-leg@127.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// RFC 3261 §9.1 at the CANCEL's sender: the SUT's own ACK is what proves
/// the transaction was over. A CANCEL AHEAD of that ACK may have crossed
/// the final in flight and is not charged; one behind it cannot have, and
/// is.
#[test]
fn a_cancel_the_sut_sent_after_acking_its_own_final_is_flagged() {
    let crossed = vec![
        sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
        recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
        sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 2),
        sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 3),
    ];
    assert!(
        live(RuleId::NoCancelAfterFinal).check_positioned(&crossed).is_empty(),
        "a CANCEL the ACK has not yet sealed may have crossed the final"
    );

    let late = vec![
        sent(SUT, b_leg_invite("z9hG4bK-b"), BOB, 0),
        recv(SUT, b_leg_resp(486, "z9hG4bK-b"), BOB, 1),
        sent(SUT, b_leg_req("ACK", "z9hG4bK-b", Some("bt")), BOB, 2),
        sent(SUT, b_leg_req("CANCEL", "z9hG4bK-b", None), BOB, 3),
    ];
    let out = live(RuleId::NoCancelAfterFinal).check_positioned(&late);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that sent the CANCEL is charged");
    assert_eq!(out[0].3.as_deref(), Some(SUT));
    assert!(out[0].1.contains("486") && out[0].1.contains("§9.1"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(4), "offending points at the late CANCEL");
    assert!(!live(RuleId::NoCancelAfterFinal).force_advisory(), "the late CANCEL gates");
}

/// The far end's own late CANCEL is not this bind's: the rule charges the
/// UAC that sent it. That UAC is no recorded bind, so the finding is
/// reported on the lane that took the CANCEL, charged to its sender.
#[test]
fn a_late_cancel_the_peer_sent_is_not_charged_to_this_bind() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(486, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        recv(SUT, req("ACK", "z9hG4bK-i", 1, Some("bt")), ALICE, 2),
        recv(SUT, req("CANCEL", "z9hG4bK-i", 1, None), ALICE, 3),
    ];
    let out = live(RuleId::NoCancelAfterFinal).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported where the CANCEL was taken");
    assert_eq!(out[0].3, Some(ALICE.to_string()), "charged to the UAC that sent it");
}

#[test]
fn divergent_cancel_route_is_flagged_at_its_wire_position() {
    let evs = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
        sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p2@h;lr>\r\n"), BOB, 1),
    ];
    let out = live(RuleId::CancelRouteEchoesInvite).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that sent the CANCEL");
    assert!(out[0].1.contains("differ") && out[0].1.contains("z9hG4bK-r"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the CANCEL");

    let echoed = vec![
        sent(SUT, req_with("INVITE", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 0),
        sent(SUT, req_with("CANCEL", "z9hG4bK-r", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
    ];
    assert!(live(RuleId::CancelRouteEchoesInvite).check_positioned(&echoed).is_empty());
}

#[test]
fn relay_lane_passing_a_cancel_through_is_not_judged() {
    // A lane that forwards both directions of one Call-ID passes the
    // upstream UAC's CANCEL through; a divergence is the upstream's.
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req_with("INVITE", "z9hG4bK-b", "Route: <sip:p1@h;lr>\r\n"), BOB, 1),
        sent(SUT, req_with("CANCEL", "z9hG4bK-b", "Route: <sip:p2@h;lr>\r\n"), BOB, 2),
    ];
    assert!(live(RuleId::CancelRouteEchoesInvite).check_positioned(&evs).is_empty());
}

#[test]
fn eager_cancel_gates_on_a_forwarding_lane() {
    // A B2BUA/AS lane authoring its own b-leg CANCEL is judged on it — no
    // relay exemption here.
    assert!(!live(RuleId::CancelAfter1xx).force_advisory());
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
    ];
    let out = live(RuleId::CancelAfter1xx).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("before any received"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3), "offending points at the CANCEL");

    // A taken provisional discharges it.
    let waited = vec![
        recv(SUT, req("INVITE", "z9hG4bK-a", 1, None), ALICE, 0),
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        recv(SUT, resp(180, 1, "INVITE", "bt", "z9hG4bK-b"), BOB, 2),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 3),
    ];
    assert!(live(RuleId::CancelAfter1xx).check_positioned(&waited).is_empty());
}

#[test]
fn proxy_declared_lane_sends_no_eager_cancel_finding() {
    // A `{Proxy}`-only lane forwards the upstream UAC's CANCEL at the
    // upstream's timing: exempt, where a `{Uac, Uas}` lane is judged.
    let wire = vec![
        sent(SUT, req("INVITE", "z9hG4bK-b", 1, None), BOB, 1),
        sent(SUT, req("CANCEL", "z9hG4bK-b", 1, None), BOB, 2),
    ];
    let mut ua = vec![bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0)];
    ua.extend(wire.clone());
    assert_eq!(live(RuleId::CancelAfter1xx).check(&ua).len(), 1, "a UA lane is judged");

    let mut proxy = vec![bind_roles(SUT, HashSet::from([UaRole::Proxy]), 0)];
    proxy.extend(wire);
    assert!(live(RuleId::CancelAfter1xx).check(&proxy).is_empty(), "a {{Proxy}} lane is exempt");
}
