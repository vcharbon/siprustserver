//! Unit tests for SDP codec-profile extraction + held SDP construction. Port
//! of `tests/sip/sdp-utils.test.ts`.

use sip_message::sdp::{
    build_held_sdp_from_profile, extract_codec_profile, BuildHeldSdpOptions, CodecProfile,
};

fn sample_sdp() -> String {
    [
        "v=0",
        "o=alice 2890844526 2890844526 IN IP4 192.0.2.10",
        "s=-",
        "c=IN IP4 192.0.2.10",
        "t=0 0",
        "m=audio 20000 RTP/AVP 8 18 101",
        "a=rtpmap:8 PCMA/8000",
        "a=rtpmap:18 G729/8000",
        "a=rtpmap:101 telephone-event/8000",
        "a=fmtp:18 annexb=no",
        "a=fmtp:101 0-15",
        "a=ptime:20",
        "a=sendrecv",
    ]
    .join("\r\n")
        + "\r\n"
}

#[test]
fn parses_m_audio_with_payload_types_and_rtpmaps() {
    let profile = extract_codec_profile(sample_sdp().as_bytes()).expect("profile");
    assert_eq!(profile.media, "audio");
    assert_eq!(profile.payload_types, vec![8, 18, 101]);
    assert_eq!(
        profile.rtpmaps,
        vec![
            "a=rtpmap:8 PCMA/8000",
            "a=rtpmap:18 G729/8000",
            "a=rtpmap:101 telephone-event/8000",
        ]
    );
    assert_eq!(profile.fmtp, vec!["a=fmtp:18 annexb=no", "a=fmtp:101 0-15"]);
    assert_eq!(profile.ptime.as_deref(), Some("a=ptime:20"));
    assert_eq!(profile.maxptime, None);
}

#[test]
fn accepts_lf_only_line_endings() {
    let lf = sample_sdp().replace("\r\n", "\n");
    let profile = extract_codec_profile(lf.as_bytes()).expect("profile");
    assert_eq!(profile.payload_types, vec![8, 18, 101]);
    assert_eq!(profile.rtpmaps.len(), 3);
}

#[test]
fn ignores_video_returns_first_audio() {
    let sdp = [
        "v=0",
        "o=- 0 0 IN IP4 0.0.0.0",
        "s=-",
        "t=0 0",
        "m=video 30000 RTP/AVP 96",
        "a=rtpmap:96 H264/90000",
        "m=audio 40000 RTP/AVP 0 8",
        "a=rtpmap:0 PCMU/8000",
        "a=rtpmap:8 PCMA/8000",
    ]
    .join("\r\n");
    let profile = extract_codec_profile(sdp.as_bytes()).expect("profile");
    assert_eq!(profile.media, "audio");
    assert_eq!(profile.payload_types, vec![0, 8]);
}

#[test]
fn stops_rtpmap_accumulation_at_next_m_section() {
    let sdp = [
        "v=0",
        "o=- 0 0 IN IP4 0.0.0.0",
        "s=-",
        "t=0 0",
        "m=audio 20000 RTP/AVP 0",
        "a=rtpmap:0 PCMU/8000",
        "m=video 30000 RTP/AVP 96",
        "a=rtpmap:96 H264/90000",
    ]
    .join("\r\n");
    let profile = extract_codec_profile(sdp.as_bytes()).expect("profile");
    assert_eq!(profile.rtpmaps, vec!["a=rtpmap:0 PCMU/8000"]);
}

#[test]
fn drops_lines_for_payload_types_not_in_m_line() {
    let sdp = [
        "v=0",
        "o=- 0 0 IN IP4 0.0.0.0",
        "s=-",
        "t=0 0",
        "m=audio 20000 RTP/AVP 8",
        "a=rtpmap:8 PCMA/8000",
        "a=rtpmap:9 G722/8000",
        "a=fmtp:9 mode=lowrate",
    ]
    .join("\r\n");
    let profile = extract_codec_profile(sdp.as_bytes()).expect("profile");
    assert_eq!(profile.payload_types, vec![8]);
    assert_eq!(profile.rtpmaps, vec!["a=rtpmap:8 PCMA/8000"]);
    assert!(profile.fmtp.is_empty());
}

#[test]
fn none_when_no_audio_m_line() {
    let sdp = ["v=0", "o=- 0 0 IN IP4 0.0.0.0", "s=-", "t=0 0", "m=video 30000 RTP/AVP 96"]
        .join("\r\n");
    assert!(extract_codec_profile(sdp.as_bytes()).is_none());
}

#[test]
fn none_on_empty_body() {
    assert!(extract_codec_profile(b"").is_none());
    assert!(extract_codec_profile(&[]).is_none());
}

fn held_opts() -> BuildHeldSdpOptions {
    BuildHeldSdpOptions { local_ip: "192.0.2.20".to_string(), now_ms: 1_700_000_000_000 }
}

#[test]
fn produces_held_sdp_with_port0_inactive_and_codecs() {
    let profile = CodecProfile {
        media: "audio".to_string(),
        payload_types: vec![8, 18, 101],
        rtpmaps: vec![
            "a=rtpmap:8 PCMA/8000".to_string(),
            "a=rtpmap:18 G729/8000".to_string(),
            "a=rtpmap:101 telephone-event/8000".to_string(),
        ],
        fmtp: vec!["a=fmtp:101 0-15".to_string()],
        ptime: Some("a=ptime:20".to_string()),
        maxptime: None,
    };
    let body = String::from_utf8(build_held_sdp_from_profile(&profile, &held_opts())).unwrap();
    assert!(body.contains("m=audio 0 RTP/AVP 8 18 101"));
    assert!(body.contains("a=rtpmap:8 PCMA/8000"));
    assert!(body.contains("a=rtpmap:18 G729/8000"));
    assert!(body.contains("a=rtpmap:101 telephone-event/8000"));
    assert!(body.contains("a=fmtp:101 0-15"));
    assert!(body.contains("a=ptime:20"));
    assert!(body.contains("a=inactive"));
    assert!(body.contains("c=IN IP4 192.0.2.20"));
    assert!(!body.contains("0.0.0.0"));
    assert!(!body.contains("o=b2bua 0 0 "));
}

#[test]
fn substitutes_loopback_for_bind_all_placeholder() {
    let profile = CodecProfile {
        media: "audio".to_string(),
        payload_types: vec![0],
        rtpmaps: vec!["a=rtpmap:0 PCMU/8000".to_string()],
        fmtp: vec![],
        ptime: None,
        maxptime: None,
    };
    let opts = BuildHeldSdpOptions { local_ip: "0.0.0.0".to_string(), now_ms: 1_700_000_000_000 };
    let body = String::from_utf8(build_held_sdp_from_profile(&profile, &opts)).unwrap();
    assert!(body.contains("c=IN IP4 127.0.0.1"));
    assert!(body.contains("o=b2bua 1700000000 1700000000 IN IP4 127.0.0.1"));
}

#[test]
fn roundtrips_through_extract_codec_profile() {
    let original = extract_codec_profile(sample_sdp().as_bytes()).unwrap();
    let held = build_held_sdp_from_profile(&original, &held_opts());
    let extracted = extract_codec_profile(&held).unwrap();
    assert_eq!(extracted.payload_types, original.payload_types);
    assert_eq!(extracted.rtpmaps, original.rtpmaps);
    assert_eq!(extracted.fmtp, original.fmtp);
    assert_eq!(extracted.ptime, original.ptime);
}

#[test]
fn omits_ptime_maxptime_when_absent() {
    let profile = CodecProfile {
        media: "audio".to_string(),
        payload_types: vec![0],
        rtpmaps: vec!["a=rtpmap:0 PCMU/8000".to_string()],
        fmtp: vec![],
        ptime: None,
        maxptime: None,
    };
    let body = String::from_utf8(build_held_sdp_from_profile(&profile, &held_opts())).unwrap();
    assert!(!body.contains("a=ptime"));
    assert!(!body.contains("a=maxptime"));
}
