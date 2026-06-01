//! Slice 1 (framing half): RTP wire framing cross-check — independent witness.
//!
//! Each framing implementation must parse the other's bytes. Port of the
//! `expectHeaderRoundTrip` matrix in `../sipjsserver/tests/media/rtp-media.test.ts`.

use media::rtp::{HandRolled, RtpFraming, RtpHeader, WebRtcRs};

const HEADER: RtpHeader = RtpHeader {
    version: 2,
    padding: false,
    extension: false,
    marker: true,
    payload_type: 8,
    sequence_number: 4321,
    timestamp: 1_234_567,
    ssrc: 0x8899_AABB,
};

fn expect_header_round_trip(enc: &dyn RtpFraming, dec: &dyn RtpFraming) {
    let payload = [10u8, 20, 30, 40, 50, 60, 70, 80];
    let wire = enc.encode_rtp(&HEADER, &payload);
    let parsed = dec
        .parse_rtp(&wire)
        .unwrap_or_else(|| panic!("{} → {} parse returned None", enc.name(), dec.name()));
    let tag = format!("{} → {}", enc.name(), dec.name());
    assert_eq!(parsed.header.payload_type, HEADER.payload_type, "{tag} pt");
    assert_eq!(
        parsed.header.sequence_number, HEADER.sequence_number,
        "{tag} seq"
    );
    assert_eq!(parsed.header.timestamp, HEADER.timestamp, "{tag} ts");
    assert_eq!(parsed.header.ssrc, HEADER.ssrc, "{tag} ssrc");
    assert!(parsed.header.marker, "{tag} marker");
    assert_eq!(parsed.payload, payload, "{tag} payload");
}

#[test]
fn ts_encode_ts_parse() {
    expect_header_round_trip(&HandRolled, &HandRolled);
}

#[test]
fn ts_encode_webrtc_parse() {
    expect_header_round_trip(&HandRolled, &WebRtcRs);
}

#[test]
fn webrtc_encode_ts_parse() {
    expect_header_round_trip(&WebRtcRs, &HandRolled);
}

#[test]
fn webrtc_encode_webrtc_parse() {
    expect_header_round_trip(&WebRtcRs, &WebRtcRs);
}

#[test]
fn non_v2_packet_is_rejected() {
    // Version 1 in the top two bits → both parsers reject.
    let mut bad = HandRolled.encode_rtp(&HEADER, &[1, 2, 3]);
    bad[0] = (1 << 6) | (bad[0] & 0x3f);
    assert!(HandRolled.parse_rtp(&bad).is_none());
    assert!(WebRtcRs.parse_rtp(&bad).is_none());
}

#[test]
fn handrolled_parses_csrc_and_payload_offset() {
    // Craft a packet with CSRC count 2: the hand-rolled parser must skip the
    // 8 CSRC bytes and return the trailing payload intact.
    let mut wire = vec![
        (2 << 6) | 0x02, // V=2, CC=2
        8,               // PT 8, no marker
        0x10,
        0x20, // seq
        0,
        0,
        0,
        100, // ts
        0,
        0,
        0,
        7, // ssrc
        0,
        0,
        0,
        1, // csrc[0]
        0,
        0,
        0,
        2, // csrc[1]
    ];
    wire.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let parsed = HandRolled.parse_rtp(&wire).expect("csrc packet parses");
    assert_eq!(parsed.payload, vec![0xAA, 0xBB, 0xCC]);
    assert_eq!(parsed.header.ssrc, 7);
}
