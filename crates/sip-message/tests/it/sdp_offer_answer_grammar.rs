//! The RFC 3264 offer/answer-model layer over the RFC 4566 §5 grammar: the
//! bounds and the hold idiom `sdp::validate_offer_answer_body` /
//! `sdp::c_line_is_unspecified` state, in the spellings a description may use.

use sip_message::sdp::{c_line_is_unspecified, validate_offer_answer_body};

/// A one-audio-stream description with the caller's `o=` line.
fn body(o_line: &str) -> Vec<u8> {
    format!("v=0\r\n{o_line}\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n")
        .into_bytes()
}

fn reason(b: &[u8]) -> String {
    validate_offer_answer_body(b).expect_err("a rejected body").reason
}

/// An empty body carries no description to reject.
#[test]
fn an_empty_body_passes() {
    assert!(validate_offer_answer_body(b"").is_ok());
}

#[test]
fn a_well_formed_description_passes() {
    assert!(validate_offer_answer_body(&body("o=alice 1 1 IN IP4 10.0.0.1")).is_ok());
}

/// The §5 grammar failures the base validator owns still surface verbatim.
#[test]
fn the_base_grammar_failures_surface() {
    let no_t = b"v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\ns=-\r\nm=audio 1 RTP/AVP 0\r\n";
    assert_eq!(reason(no_t), "missing t= line");
}

/// A `v=` line that is not the version, and a second session in one body.
#[test]
fn the_session_count_and_version_are_bounded() {
    assert!(reason(b"o=x\r\nm=audio 1 RTP/AVP 0\r\n").contains("missing v= line"));
    let two = b"v=0\r\nv=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nc=IN IP4 h\r\nt=0 0\r\n";
    assert!(reason(two).contains("2 session descriptions"), "{}", reason(two));
    let v1 = b"v=1\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nc=IN IP4 h\r\nt=0 0\r\n";
    assert!(reason(v1).contains("unexpected v= value"), "{}", reason(v1));
}

/// sess-id and sess-version are non-negative integers, read as opaque digit
/// strings: RFC 4566 §5.2 bounds neither below 64 bits, so a full-width NTP
/// timestamp is legal and only a non-digit spelling is rejected.
#[test]
fn the_origin_integers_are_digit_strings_of_any_width() {
    assert!(reason(&body("o=alice -1 1 IN IP4 h")).contains("sess-id '-1' is not"));
    assert!(reason(&body("o=alice 1 x IN IP4 h")).contains("sess-version 'x' is not"));
    assert!(reason(&body("o=alice 1. 1 IN IP4 h")).contains("sess-id '1.' is not"));
    assert!(reason(&body("o=alice 1 1e9 IN IP4 h")).contains("sess-version '1e9' is not"));
}

/// The NTP-magnitude `sess-version` a real endpoint sends — above `2^53 - 1`,
/// and legal.
#[test]
fn a_sixty_four_bit_session_version_passes() {
    let o = "o=- 2170552860 2736569745311754808 IN IP4 203.0.113.17";
    assert!(validate_offer_answer_body(&body(o)).is_ok());
    // Past u64 as well: the field is opaque, so no width truncates it.
    let past_u64 = "o=- 18446744073709551616 18446744073709551617 IN IP4 h";
    assert!(validate_offer_answer_body(&body(past_u64)).is_ok());
}

/// A stream needs a `c=` — its own or the session's — and an integer port.
#[test]
fn every_stream_needs_an_address_and_a_port() {
    let no_c = b"v=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
    assert!(reason(no_c).contains("m=audio block has no c= line"), "{}", reason(no_c));

    let own_c =
        b"v=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\nm=audio 1 RTP/AVP 0\r\nc=IN IP4 h\r\n";
    assert!(validate_offer_answer_body(own_c).is_ok());

    // The FIRST block's c= does not carry over to the second.
    let second_bare = b"v=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nt=0 0\r\n\
                        m=audio 1 RTP/AVP 0\r\nc=IN IP4 h\r\nm=video 2 RTP/AVP 96\r\n";
    assert!(reason(second_bare).contains("m=video block has no c= line"));

    let bad_port = b"v=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nc=IN IP4 h\r\nt=0 0\r\n\
                     m=audio -1 RTP/AVP 0\r\n";
    assert!(reason(bad_port).contains("m=audio line states no port field"), "{}", reason(bad_port));
}

/// A description with one audio stream and the caller's `m=` port field and
/// trailing media-level lines.
fn stream(port_field: &str, media_lines: &str) -> Vec<u8> {
    format!(
        "v=0\r\no=alice 1 1 IN IP4 h\r\ns=-\r\nc=IN IP4 h\r\nt=0 0\r\n\
         m=video {port_field} RTP/AVP 31\r\n{media_lines}"
    )
    .into_bytes()
}

/// RFC 8866 §5.14: `<port>/<number of ports>` states consecutive port pairs,
/// a rejected stream included.
#[test]
fn a_port_field_may_state_a_number_of_ports() {
    assert!(validate_offer_answer_body(&stream("49170/2", "")).is_ok());
    assert!(validate_offer_answer_body(&stream("0/2", "")).is_ok());
    for off in ["49170/0", "49170/x", "49170/", "49170x"] {
        assert!(reason(&stream(off, "")).contains("states no port field"), "{off}");
    }
}

/// RFC 8866 §6.4: a packet time is a `non-zero-int-or-real`, so a decimal
/// one is legal and zero is not.
#[test]
fn ptime_is_a_non_zero_integer_or_real() {
    for valid in ["a=ptime:20\r\n", "a=ptime:2.5\r\n", "a=ptime:0.5\r\n"] {
        assert!(validate_offer_answer_body(&stream("1", valid)).is_ok(), "{valid}");
    }
    assert_eq!(
        reason(&stream("1", "a=ptime:0\r\n")),
        "a=ptime value \"0\" is not a non-zero integer or real (RFC 8866 §6.4)"
    );
    assert!(reason(&stream("1", "a=ptime:20.0\r\n")).contains("value \"20.0\" is not"));
    let spaced = reason(&stream("1", "a=ptime:20 \r\n"));
    assert!(spaced.contains("value \"20 \" is not"), "the value is quoted as written: {spaced}");
}

/// RFC 8866 §5: the session-level lines precede the first `m=`, so an `o=`
/// or a `t=` inside a media description is out of order.
#[test]
fn a_session_level_line_after_media_is_refused() {
    let late_origin = stream("1", "o=alice 2 2 IN IP4 h\r\n");
    assert!(reason(&late_origin).contains("'o=alice 2 2 IN IP4 h' follows an m= line"));
    let late_time = stream("1", "t=0 0\r\n");
    assert!(reason(&late_time).contains("'t=0 0' follows an m= line"));
    assert!(validate_offer_answer_body(&stream("1", "i=title\r\nb=AS:64\r\n")).is_ok());
}

/// The §8.4 hold idiom, in both address families and with the trailing
/// `/ttl` / `/count` the grammar allows on the address. RFC 4291 §2.2 leaves
/// the IPv6 zero run free to compress or not, so both spellings name it.
#[test]
fn the_unspecified_address_is_recognised_in_both_families() {
    for held in [
        "IN IP4 0.0.0.0",
        "in ip4 0.0.0.0/127",
        "IN IP6 ::",
        "IN IP6 ::/2",
        "IN IP6 0:0:0:0:0:0:0:0",
        "IN IP6 0:0:0:0:0:0:0:0/2",
        "IN IP4 0.0.0.0 x",
    ] {
        assert!(c_line_is_unspecified(held), "{held}");
    }
    for live in [
        "IN IP4 10.0.0.1",
        "IN IP6 ::1",
        "IN IP6 0:0:0:0:0:0:0:1",
        "IN IP4 0.0.0.00",
        "IN IP4",
        "XX IP4 0.0.0.0",
    ] {
        assert!(!c_line_is_unspecified(live), "{live}");
    }
}
