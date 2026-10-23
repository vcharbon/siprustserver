//! The live surfacing of the retransmission family rules.

use super::*;

/// The same datagram with its From and To rows swapped: every header the
/// first copy carried, and not the same response.
fn recomposed(raw: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8(raw).unwrap();
    let mut rows: Vec<&str> = text.split("\r\n").collect();
    rows.swap(2, 3);
    rows.join("\r\n").into_bytes()
}

/// The violation, at its wire position: the SUT answered 200 and its
/// repeat was re-composed rather than repeated.
#[test]
fn a_re_composed_2xx_rung_is_charged_to_the_lane_that_sent_it() {
    let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, ok.clone(), ALICE, 1),
        sent(SUT, recomposed(ok), ALICE, 2),
    ];
    let out = live(RuleId::RungByteIdentical).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "attributed to the lane that re-composed its rung");
    assert_eq!(out[0].3.as_deref(), Some(SUT));
    let detail = &out[0].1;
    assert!(detail.contains("2xx final") && detail.contains("200 (INVITE)"), "{detail}");
    assert!(detail.contains("z9hG4bK-i") && detail.contains("cid-1@127.0.0.1"), "{detail}");
    assert!(detail.contains("copy 1 of 2") && detail.contains("in the head at byte"), "{detail}");
    assert!(detail.contains("first `From: <sip:alice@127.0.0.1>;tag=at`"), "{detail}");
    assert!(detail.contains("this copy `To: <sip:bob@127.0.0.1>;tag=bt`"), "{detail}");
    assert!(detail.contains("§13.3.1.4"), "{detail}");
    assert_eq!(out[0].2, Some(3), "offending points at the divergent copy");
    assert!(!live(RuleId::RungByteIdentical).force_advisory(), "a divergent rung gates");
}

/// The LB proxy's own Record-Route cookie may differ between two copies of
/// one forwarded INVITE: the lane key IS the emitter's `ip:port`, spelled
/// as the proxy's Record-Route URI spells it, so the rule masks that row —
/// and no other.
#[test]
fn a_re_stamped_own_record_route_cookie_is_not_charged_but_a_foreign_row_is() {
    const WORKER: &str = "127.0.0.1:5091";
    let forwarded = |cookie: &str, foreign: &str| {
        format!(
            "INVITE {B} SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-p;rport\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i;received=127.0.0.1;rport=5060\r\n\
                 Record-Route: <sip:127.0.0.1:5080;outbound;lr>\r\n\
                 Record-Route: <sip:127.0.0.1:5080;w_pri=b1;{cookie};lr>\r\n\
                 Record-Route: <sip:10.0.0.7:5060;{foreign}>\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Max-Forwards: 69\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, forwarded("w_bak=b2", "lr"), WORKER, 1),
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
        sent(SUT, forwarded("w_bak", "lr"), WORKER, 3),
    ];
    assert!(live(RuleId::RungByteIdentical).check_positioned(&evs).is_empty());

    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, forwarded("w_bak=b2", "lr"), WORKER, 1),
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 2),
        sent(SUT, forwarded("w_bak", "x=1;lr"), WORKER, 3),
    ];
    let out = live(RuleId::RungByteIdentical).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    let detail = &out[0].1;
    assert!(detail.contains("INVITE request"), "{detail}");
    assert!(detail.contains("first `Record-Route: <sip:10.0.0.7:5060;lr>`"), "{detail}");
    assert!(detail.contains("this copy `Record-Route: <sip:10.0.0.7:5060;x=1;lr>`"), "{detail}");
}

/// The obliged behaviour: the same bytes again is nothing to report, and
/// the projection's repeat mark does not change that.
#[test]
fn a_byte_identical_2xx_rung_is_not_charged() {
    let ok = resp(200, 1, "INVITE", "bt", "z9hG4bK-i");
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, ok.clone(), ALICE, 1),
        sent(SUT, ok.clone(), ALICE, 2),
        sent(SUT, ok, ALICE, 3),
    ];
    assert!(live(RuleId::RungByteIdentical).check_positioned(&evs).is_empty());
}

/// A B2BUA relaying two forks' 2xx sends two messages under one branch and
/// CSeq; the tag tells them apart, and neither is a rung of the other.
#[test]
fn another_forks_2xx_is_not_a_rung() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, resp(200, 1, "INVITE", "bt", "z9hG4bK-i"), ALICE, 1),
        sent(SUT, resp(200, 1, "INVITE", "bt2", "z9hG4bK-i"), ALICE, 2),
    ];
    assert!(live(RuleId::RungByteIdentical).check_positioned(&evs).is_empty());
}

/// The far end's re-composed rung is not this bind's: the rule charges the
/// emitter, which is no recorded bind, so the finding is reported where the
/// rungs were taken, charged to their sender.
#[test]
fn a_re_composed_rung_the_peer_sent_is_not_charged_to_this_bind() {
    let bye = req("BYE", "z9hG4bK-b", 2, Some("bt"));
    let evs = vec![recv(SUT, bye.clone(), ALICE, 0), recv(SUT, recomposed(bye), ALICE, 1)];
    let out = live(RuleId::RungByteIdentical).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert_eq!(out[0].3, Some(ALICE.to_string()));
}

/// A reliable provisional the SUT re-sent under the same `RSeq` with other
/// bytes is charged under RFC 3262 §3; the next `RSeq` is another message.
#[test]
fn a_re_composed_reliable_provisional_rung_is_charged() {
    let reliable = |rseq: u32| {
        format!(
            "SIP/2.0 183 Session Progress\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
                 From: <{A}>;tag=at\r\n\
                 To: <{B}>;tag=bt\r\n\
                 Call-ID: cid-1@127.0.0.1\r\n\
                 CSeq: 1 INVITE\r\n\
                 Require: 100rel\r\n\
                 RSeq: {rseq}\r\n\
                 Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    };
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        sent(SUT, reliable(1), ALICE, 1),
        sent(SUT, recomposed(reliable(1)), ALICE, 2),
        sent(SUT, reliable(2), ALICE, 3),
    ];
    let out = live(RuleId::RungByteIdentical).check_positioned(&evs);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("reliable provisional 183 RSeq 1 (INVITE)"), "{}", out[0].1);
    assert!(out[0].1.contains("RFC 3262 §3"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(3));
}
