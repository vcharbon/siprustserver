//! The live surfacing of the Via family rules.

use super::*;

/// RFC 3581 §4 at the taker, and it never gates.
#[test]
fn a_dropped_rport_is_advisory_and_reported_at_the_taker() {
    assert!(live(RuleId::RportEcho).force_advisory());
    let ask = |tail: &str| {
        format!(
            "OPTIONS {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Max-Forwards: 70\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let answer = |tail: &str| {
        format!(
            "SIP/2.0 200 OK\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-o{tail}\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 5 OPTIONS\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(""), BOB, 1)];
    let out = live(RuleId::RportEcho).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that asked");
    assert!(out[0].1.contains("dropped the rport"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(2), "offending points at the response");

    let echoed = vec![sent(SUT, ask(";rport"), BOB, 0), recv(SUT, answer(";rport=5080"), BOB, 1)];
    assert!(live(RuleId::RportEcho).check_positioned(&echoed).is_empty());
}
