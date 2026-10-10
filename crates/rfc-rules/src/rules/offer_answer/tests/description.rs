//! A description on its own bytes.

use super::*;

// ---- the two per-description obligations ----------------------------

/// An INVITE alice sent, its body declared under `content_type`.
fn declared(body: &str, content_type: &str) -> Msg {
    let mut m = req(1, ALICE, BOB, "INVITE", 1, None, Some(body));
    m.head = Some(format!("Content-Type: {content_type}\r\n\r\n").into_bytes());
    m
}

const GOOD_SDP: &str = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=ptime:20\r\n";

#[test]
fn a_well_formed_description_is_compliant() {
    let f = run(&SdpBodyParseable, &[declared(GOOD_SDP, "application/sdp")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The `o=` fields are digit strings of any width: an NTP-magnitude
/// `sess-version` is what RFC 4566 §5.2 recommends, not a defect.
#[test]
fn a_sixty_four_bit_session_version_is_compliant() {
    let body = "v=0\r\no=- 2170552860 2736569745311754808 IN IP4 203.0.113.17\r\ns=-\r\n\
                    c=IN IP4 203.0.113.17\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
    let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// The grammar walk's own failures, each surfacing verbatim.
#[test]
fn every_grammar_failure_is_named_on_its_message() {
    let cases = [
        (
            "v=0\r\nv=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\n",
            "session descriptions",
        ),
        (
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=ptime:0\r\n",
            "ptime",
        ),
        (
            "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n\
                 m=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
            "no c= line",
        ),
    ];
    for (body, needle) in cases {
        let f = run(&SdpBodyParseable, &[declared(body, "application/sdp")]);
        let Decision::Violated(Evidence::SdpBodyRejected { sdp_failure, .. }) = &f[0].decision
        else {
            panic!("sdp evidence: {:?}", f[0].decision)
        };
        assert!(sdp_failure.contains(needle), "{sdp_failure} lacks {needle}");
    }
}

/// A body under another format is not this rule's to read, and neither is a
/// message that carried none.
#[test]
fn an_undeclared_or_bodiless_message_is_no_occasion() {
    assert!(run(&SdpBodyParseable, &[declared("not sdp at all", "text/plain")]).is_empty());
    assert!(run(&SdpBodyParseable, &[declared("", "application/sdp")]).is_empty());
    let mut headless = declared(GOOD_SDP, "application/sdp");
    headless.head = None;
    assert!(run(&SdpBodyParseable, &[headless]).is_empty());
}

/// Held (`c=0.0.0.0`) AND rejected (port 0) at once, at session level and
/// at media level.
#[test]
fn a_held_and_rejected_stream_is_violated() {
    let session = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                       m=audio 0 RTP/AVP 0\r\n";
    let media = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
                     m=audio 0 RTP/AVP 0\r\nc=IN IP4 0.0.0.0\r\n";
    for body in [session, media] {
        let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
        let Decision::Violated(Evidence::HeldAndRejectedStreams {
            held_stream_indexes,
            held_streams,
            held_c_lines,
            ..
        }) = &f[0].decision
        else {
            panic!("held/rejected evidence: {:?}", f[0].decision)
        };
        assert_eq!(held_stream_indexes.as_slice(), [0]);
        assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP"]);
        assert!(held_c_lines[0].contains("0.0.0.0"), "{:?}", held_c_lines);
    }
}

/// Held with a live port, rejected at a real address, and a block whose own
/// `c=` overrides an unspecified session one — none is the ambiguity.
#[test]
fn one_disposition_at_a_time_is_compliant() {
    let bodies = [
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 49170 RTP/AVP 0\r\n",
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\n",
        "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             m=audio 0 RTP/AVP 0\r\nc=IN IP4 10.0.0.1\r\n",
    ];
    for body in bodies {
        let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{body}: {:?}", f[0].decision);
    }
}

/// Two contradictory streams in one body are ONE finding naming both.
#[test]
fn every_offending_stream_rides_one_finding() {
    let body = "v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
                    m=audio 0 RTP/AVP 0\r\nm=video 0 RTP/AVP 96\r\n";
    let f = run(&C0PortNonZero, &[declared(body, "application/sdp")]);
    assert_eq!(f.len(), 1, "one description, one finding: {f:?}");
    let Decision::Violated(Evidence::HeldAndRejectedStreams {
        held_stream_indexes,
        held_streams,
        ..
    }) = &f[0].decision
    else {
        panic!("held/rejected evidence: {:?}", f[0].decision)
    };
    assert_eq!(held_stream_indexes.as_slice(), [0, 1]);
    assert_eq!(held_streams.as_slice(), ["audio 0 RTP/AVP", "video 0 RTP/AVP"]);
}

/// A message carrying no description at all opens no occasion.
#[test]
fn a_bodiless_message_is_no_c0_occasion() {
    assert!(run(&C0PortNonZero, &[req(1, ALICE, BOB, "BYE", 2, Some("bt"), None)]).is_empty());
}
