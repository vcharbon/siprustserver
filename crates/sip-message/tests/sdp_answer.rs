//! Unit tests for SDP answer-from-offer construction. Port of
//! `tests/sip/sdp-answer-from-offer.test.ts`.

use sip_message::sdp::{build_answer_from_offer, BuildAnswerOptions, SdpBuildResult};

fn sdp(lines: &[&str]) -> String {
    lines.join("\r\n") + "\r\n"
}

fn opts() -> BuildAnswerOptions {
    BuildAnswerOptions { local_ip: "192.0.2.10".to_string(), now_ms: 1_700_000_000_000 }
}

fn alice_basic() -> String {
    sdp(&[
        "v=0",
        "o=alice 1 1 IN IP4 192.0.2.1",
        "s=-",
        "c=IN IP4 192.0.2.1",
        "t=0 0",
        "m=audio 6000 RTP/AVP 8 18 101",
        "a=rtpmap:8 PCMA/8000",
        "a=rtpmap:18 G729/8000",
        "a=rtpmap:101 telephone-event/8000",
        "a=sendrecv",
    ])
}

fn ok_body(result: SdpBuildResult) -> String {
    match result {
        SdpBuildResult::Ok(b) => String::from_utf8(b).unwrap(),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn intersects_codecs_keeps_bobs_pt_numbering() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 0 8 96",
        "a=rtpmap:0 PCMU/8000",
        "a=rtpmap:8 PCMA/8000",
        "a=rtpmap:96 opus/48000/2",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(
        bob.as_bytes(),
        Some(alice_basic().as_bytes()),
        &opts(),
    ));
    assert!(body.contains("m=audio 6000 RTP/AVP 8"));
    assert!(body.contains("a=rtpmap:8 PCMA/8000"));
    assert!(!body.contains("a=rtpmap:0 PCMU/8000"));
    assert!(!body.contains("a=rtpmap:96 opus"));
    assert!(body.contains("c=IN IP4 192.0.2.1"));
    assert!(body.contains("a=sendrecv"));
}

#[test]
fn matches_by_codec_name_when_pt_numbers_differ() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 96",
        "a=rtpmap:96 opus/48000/2",
        "a=sendrecv",
    ]);
    let alice = sdp(&[
        "v=0",
        "o=alice 1 1 IN IP4 192.0.2.1",
        "s=-",
        "c=IN IP4 192.0.2.1",
        "t=0 0",
        "m=audio 7000 RTP/AVP 111",
        "a=rtpmap:111 opus/48000/2",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(bob.as_bytes(), Some(alice.as_bytes()), &opts()));
    assert!(body.contains("m=audio 7000 RTP/AVP 96"));
    assert!(body.contains("a=rtpmap:96 opus/48000/2"));
    assert!(!body.contains("a=rtpmap:111"));
}

#[test]
fn recognises_static_pts_without_rtpmap() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 0 8",
        "a=sendrecv",
    ]);
    let alice = sdp(&[
        "v=0",
        "o=alice 1 1 IN IP4 192.0.2.1",
        "s=-",
        "c=IN IP4 192.0.2.1",
        "t=0 0",
        "m=audio 7000 RTP/AVP 0 8",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(bob.as_bytes(), Some(alice.as_bytes()), &opts()));
    assert!(body.contains("m=audio 7000 RTP/AVP 0 8"));
    assert!(!body.contains("a=rtpmap:"));
}

#[test]
fn preserves_bobs_fmtp_for_matched_pts() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8 101",
        "a=rtpmap:8 PCMA/8000",
        "a=rtpmap:101 telephone-event/8000",
        "a=fmtp:101 0-15",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(
        bob.as_bytes(),
        Some(alice_basic().as_bytes()),
        &opts(),
    ));
    assert!(body.contains("a=fmtp:101 0-15"));
}

#[test]
fn inherits_bobs_direction() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendonly",
    ]);
    let body = ok_body(build_answer_from_offer(
        bob.as_bytes(),
        Some(alice_basic().as_bytes()),
        &opts(),
    ));
    assert!(body.contains("a=sendonly"));
}

#[test]
fn uses_session_level_c_when_m_line_has_none() {
    let alice_session_only = sdp(&[
        "v=0",
        "o=alice 1 1 IN IP4 192.0.2.1",
        "s=-",
        "c=IN IP4 192.0.2.99",
        "t=0 0",
        "m=audio 6000 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
    ]);
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(
        bob.as_bytes(),
        Some(alice_session_only.as_bytes()),
        &opts(),
    ));
    assert!(body.contains("c=IN IP4 192.0.2.99"));
}

#[test]
fn missing_bob_m_section_in_alice_becomes_port0_inactive() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
        "m=video 5006 RTP/AVP 96",
        "a=rtpmap:96 H264/90000",
        "a=sendrecv",
    ]);
    let body = ok_body(build_answer_from_offer(
        bob.as_bytes(),
        Some(alice_basic().as_bytes()),
        &opts(),
    ));
    assert!(body.contains("m=audio 6000 RTP/AVP 8"));
    assert!(body.contains("m=video 0 RTP/AVP 96"));
    assert!(body.contains("a=rtpmap:96 H264/90000"));
    assert!(body.contains("a=inactive"));
}

#[test]
fn no_common_codec_returns_offending_index() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 96",
        "a=rtpmap:96 opus/48000/2",
        "a=sendrecv",
    ]);
    let result = build_answer_from_offer(bob.as_bytes(), Some(alice_basic().as_bytes()), &opts());
    assert_eq!(result, SdpBuildResult::NoCommonCodec { m_line_index: 0 });
}

#[test]
fn no_common_codec_on_second_m_line_reported_with_index_1() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
        "m=audio 5006 RTP/AVP 96",
        "a=rtpmap:96 opus/48000/2",
        "a=sendrecv",
    ]);
    let alice_two_audio = sdp(&[
        "v=0",
        "o=alice 1 1 IN IP4 192.0.2.1",
        "s=-",
        "c=IN IP4 192.0.2.1",
        "t=0 0",
        "m=audio 6000 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
        "m=audio 6002 RTP/AVP 0",
        "a=rtpmap:0 PCMU/8000",
        "a=sendrecv",
    ]);
    let result =
        build_answer_from_offer(bob.as_bytes(), Some(alice_two_audio.as_bytes()), &opts());
    assert_eq!(result, SdpBuildResult::NoCommonCodec { m_line_index: 1 });
}

#[test]
fn null_alice_sdp_returns_no_alice_sdp() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
    ]);
    assert_eq!(build_answer_from_offer(bob.as_bytes(), None, &opts()), SdpBuildResult::NoAliceSdp);
}

#[test]
fn empty_alice_body_returns_no_alice_sdp() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
    ]);
    assert_eq!(
        build_answer_from_offer(bob.as_bytes(), Some(b""), &opts()),
        SdpBuildResult::NoAliceSdp
    );
    assert_eq!(
        build_answer_from_offer(bob.as_bytes(), Some(&[]), &opts()),
        SdpBuildResult::NoAliceSdp
    );
}

#[test]
fn accepts_lf_only_line_endings() {
    let bob = sdp(&[
        "v=0",
        "o=bob 2 2 IN IP4 192.0.2.2",
        "s=-",
        "c=IN IP4 192.0.2.2",
        "t=0 0",
        "m=audio 5004 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=sendrecv",
    ])
    .replace("\r\n", "\n");
    let alice = alice_basic().replace("\r\n", "\n");
    let body = ok_body(build_answer_from_offer(bob.as_bytes(), Some(alice.as_bytes()), &opts()));
    assert!(body.contains("m=audio 6000 RTP/AVP 8"));
}
