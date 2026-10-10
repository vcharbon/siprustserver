//! The live surfacing of the REGISTER family rules.

use super::*;

/// A REGISTER the lane SENT, carrying a Route header — §10.2 owes none.
#[test]
fn a_register_carrying_route_is_flagged_on_the_lane_that_sent_it() {
    let evs = vec![sent(SUT, req_with("REGISTER", "z9hG4bK-r", "Route: <sip:p@h;lr>\r\n"), BOB, 0)];
    let out = live(RuleId::RegisterNoRouteSet).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Route"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(1), "offending points at the REGISTER");

    let clean = vec![sent(SUT, req_with("REGISTER", "z9hG4bK-r", ""), BOB, 0)];
    assert!(live(RuleId::RegisterNoRouteSet).check_positioned(&clean).is_empty());
}

/// §10.2 serialisation live: a second binding for one AOR while the first
/// is unanswered, reported on the sending lane.
#[test]
fn a_racing_register_is_flagged_on_the_lane_that_sent_it() {
    let register = |branch: &str, contact: &str| {
        format!(
            "REGISTER {A} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch={branch}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{A}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 REGISTER\r\n\
                 Contact: {contact}\r\n\
                 Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
        sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 1),
    ];
    let out = live(RuleId::SerialRegister).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("still pending"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the second REGISTER");

    // Answered first: the next binding is free to differ.
    let serialised = vec![
        sent(SUT, register("z9hG4bK-1", "<sip:a@1>"), BOB, 0),
        recv(SUT, resp(200, 1, "REGISTER", "rt", "z9hG4bK-1"), BOB, 1),
        sent(SUT, register("z9hG4bK-2", "<sip:a@2>"), BOB, 2),
    ];
    assert!(live(RuleId::SerialRegister).check_positioned(&serialised).is_empty());
}
