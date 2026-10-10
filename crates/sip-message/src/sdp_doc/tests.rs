use super::*;

const AUDIO_OFFER: &[u8] = b"v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n";

#[test]
fn single_audio_m_line_is_lifted() {
    let doc = parse_sdp_body(AUDIO_OFFER).expect("sdp");
    assert_eq!(doc.version.as_deref(), Some("0"));
    assert_eq!(doc.origin.as_deref(), Some("alice 2890844526 2890844526 IN IP4 10.0.0.1"));
    assert_eq!(doc.session_name.as_deref(), Some("-"));
    assert_eq!(doc.t_line.as_deref(), Some("0 0"));
    assert_eq!(doc.c_line.as_deref(), Some("IN IP4 10.0.0.1"));
    assert_eq!(doc.media.len(), 1);

    let m = &doc.media[0];
    assert_eq!(m.r#type, "audio");
    assert_eq!(m.port, Some(49170));
    assert_eq!(m.transport, "RTP/AVP");
    assert_eq!(m.formats, vec!["0", "8", "96"]);
    assert_eq!(m.ptime, Some(20));
    assert_eq!(
        m.attributes,
        vec!["rtpmap:0 PCMU/8000", "rtpmap:96 opus/48000/2", "ptime:20", "sendrecv"]
    );
    // No media-level c= — the session-level one stands for this stream.
    assert_eq!(m.c_line, None);

    assert_eq!(extract_direction(m), SdpDirection::SendRecv);
    assert_eq!(
        extract_rtpmaps(m),
        vec![
            ("0".to_string(), "PCMU/8000".to_string()),
            ("96".to_string(), "opus/48000/2".to_string()),
        ]
    );
}

#[test]
fn two_m_line_offer_keeps_order_and_blocks() {
    let body = b"v=0\r\n\
o=bob 1 1 IN IP4 192.0.2.1\r\n\
s=session\r\n\
t=0 0\r\n\
m=audio 5000 RTP/AVP 0\r\n\
c=IN IP4 192.0.2.1\r\n\
a=sendonly\r\n\
m=video 5002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=recvonly\r\n";
    let doc = parse_sdp_body(body).expect("sdp");
    assert_eq!(doc.c_line, None, "no session-level c=");
    assert_eq!(doc.media.len(), 2);

    let audio = &doc.media[0];
    assert_eq!(audio.r#type, "audio");
    assert_eq!(audio.port, Some(5000));
    assert_eq!(audio.c_line.as_deref(), Some("IN IP4 192.0.2.1"));
    assert_eq!(extract_direction(audio), SdpDirection::SendOnly);

    let video = &doc.media[1];
    assert_eq!(video.r#type, "video");
    assert_eq!(video.port, Some(5002));
    assert_eq!(video.formats, vec!["96"]);
    assert_eq!(video.c_line, None);
    assert_eq!(extract_direction(video), SdpDirection::RecvOnly);
    assert_eq!(extract_rtpmaps(video), vec![("96".to_string(), "H264/90000".to_string())]);
}

#[test]
fn an_audio_rtpmap_reads_an_absent_channel_count_as_one() {
    assert_eq!(canonical_rtpmap("audio", "PCMA/8000"), "PCMA/8000/1");
    assert_eq!(canonical_rtpmap("AUDIO", "PCMA/8000"), "PCMA/8000/1");
    assert_eq!(canonical_rtpmap("audio", "PCMA/8000/1"), "PCMA/8000/1");
    assert_eq!(canonical_rtpmap("audio", "opus/48000/2"), "opus/48000/2");
}

#[test]
fn a_non_audio_rtpmap_and_an_unreadable_one_keep_what_was_written() {
    assert_eq!(canonical_rtpmap("video", "H264/90000"), "H264/90000");
    assert_eq!(canonical_rtpmap("application", "H224/4800"), "H224/4800");
    assert_eq!(canonical_rtpmap("audio", "PCMA"), "PCMA");
    assert_eq!(canonical_rtpmap("audio", "PCMA/"), "PCMA/");
    assert_eq!(canonical_rtpmap("audio", "/8000"), "/8000");
}

#[test]
fn rejected_stream_has_port_zero() {
    let body = b"v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=inactive\r\n";
    let doc = parse_sdp_body(body).expect("sdp");
    assert_eq!(doc.media.len(), 1);
    assert_eq!(doc.media[0].port, Some(0), "rejected stream carries port 0");
    assert_eq!(extract_direction(&doc.media[0]), SdpDirection::Inactive);
}

#[test]
fn non_sdp_body_yields_none() {
    assert!(parse_sdp_body(b"").is_none());
    assert!(parse_sdp_body(b"not sdp at all").is_none());
    assert!(parse_sdp_body(b"o=- 0 0 IN IP4 127.0.0.1\r\n").is_none());
}

#[test]
fn unparseable_port_is_none_not_zero() {
    let body = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio xyz RTP/AVP 0\r\n";
    let doc = parse_sdp_body(body).expect("sdp");
    assert_eq!(doc.media[0].port, None, "no digits read as no port, never as zero");
}

#[test]
fn extract_format_list_drops_head_tokens() {
    assert_eq!(extract_format_list("audio 49170 RTP/AVP 0 8 96"), vec!["0", "8", "96"]);
    assert!(extract_format_list("audio 49170 RTP/AVP").is_empty());
    assert!(extract_format_list("audio").is_empty());
}

#[test]
fn origin_lifts_its_six_fields_and_the_rest_of_the_body() {
    let o = parse_origin(AUDIO_OFFER).expect("origin");
    assert_eq!(o.username, "alice");
    assert_eq!(o.session_id, "2890844526");
    assert_eq!(o.session_version, 2890844526);
    assert_eq!(o.nettype, "IN");
    assert_eq!(o.addrtype, "IP4");
    assert_eq!(o.unicast_address, "10.0.0.1");
    assert!(!o.body_excluding_origin.contains("o="));
    assert!(o.body_excluding_origin.contains("m=audio"));
}

#[test]
fn origin_rejects_what_is_not_a_v0_description() {
    assert!(parse_origin(b"").is_none());
    assert!(parse_origin(b"not sdp").is_none());
    assert!(parse_origin(b"v=0\r\ns=-\r\n").is_none(), "no o= line");
    assert!(parse_origin(b"v=0\r\no=alice 1 1 IN\r\n").is_none(), "fewer than six fields");
    assert!(parse_origin(b"v=0\r\no=alice 1 x IN IP4 1.2.3.4\r\n").is_none(), "no version");
}

#[test]
fn same_session_is_the_five_fields_but_the_version() {
    let a = parse_origin(b"v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\n").expect("a");
    let bumped = parse_origin(b"v=0\r\no=alice 1 9 IN IP4 10.0.0.1\r\n").expect("b");
    let moved = parse_origin(b"v=0\r\no=alice 1 1 IN IP4 10.0.0.2\r\n").expect("c");
    assert!(a.identifies_same_session(&bumped), "only the version rose");
    assert!(!a.identifies_same_session(&moved), "the address is part of the identity");
}

#[test]
fn the_next_version_never_wraps() {
    let a = parse_origin(b"v=0\r\no=alice 1 7 IN IP4 10.0.0.1\r\n").expect("a");
    assert_eq!(a.next_version_line().as_deref(), Some("o=alice 1 8 IN IP4 10.0.0.1"));
    let top = format!("v=0\r\no=alice 1 {} IN IP4 10.0.0.1\r\n", u64::MAX);
    assert_eq!(parse_origin(top.as_bytes()).expect("top").next_version_line(), None);
}
