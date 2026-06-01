//! Slice 2: RFC 3264/3262/5009 offer-answer engine conformance.
//!
//! Port of `../sipjsserver/tests/media/sdp-negotiation.test.ts`. Positive paths
//! (codec selection, direction, hold, provisional reliability) and negative
//! paths (each a typed `SdpRule` per RFC clause), plus the P-Early-Media gate.

use media::sdp::negotiator::{is_early_media_authorized, OfferAnswerEngine, SdpRule};
use media::sdp::{parse_sdp, MediaDirection, NegotiationState};
use media::{CodecDesc, NetAddr, PCMA, PCMU};

fn engine(ip: &str, port: u16, codecs: Vec<CodecDesc>) -> OfferAnswerEngine {
    OfferAnswerEngine::new(NetAddr::new(ip, port), codecs)
}

/// Build a minimal audio answer/offer SDP with the given payload types.
fn audio_sdp(ip: &str, port: u16, pts: &[(u8, &str)], direction: &str, m_lines: usize) -> String {
    let mut s = format!("v=0\r\no=peer 1 1 IN IP4 {ip}\r\ns=-\r\nc=IN IP4 {ip}\r\nt=0 0\r\n");
    for _ in 0..m_lines {
        let fmts: Vec<String> = pts.iter().map(|(pt, _)| pt.to_string()).collect();
        s.push_str(&format!("m=audio {port} RTP/AVP {}\r\n", fmts.join(" ")));
        for (pt, name) in pts {
            s.push_str(&format!("a=rtpmap:{pt} {name}/8000\r\n"));
        }
        s.push_str(&format!("a={direction}\r\n"));
    }
    s
}

// ---- positive ----

#[test]
fn answerer_honours_offerer_codec_preference() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA, PCMU]);
    let offer = offerer.local_offer();
    assert_eq!(offer.media[0].formats, vec![PCMA.payload_type, PCMU.payload_type]);

    let mut answerer = engine("10.0.0.2", 40002, vec![PCMU, PCMA]);
    let answer = answerer.answer_to(&offer).expect("answer");
    // Offerer listed PCMA first → answerer must pick PCMA despite its own order.
    assert_eq!(answer.media[0].formats, vec![PCMA.payload_type]);
    assert_eq!(answerer.negotiated().unwrap().codec, PCMA);
    assert_eq!(answerer.state(), NegotiationState::Committed);
}

#[test]
fn uac_applies_a_final_answer_and_commits() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer();
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "sendrecv", 1));
    let neg = offerer.apply_remote(&answer, true).expect("apply");
    assert!(neg.send && neg.receive);
    assert_eq!(neg.remote, NetAddr::new("10.0.0.2", 40002));
    assert_eq!(offerer.state(), NegotiationState::Committed);
}

#[test]
fn provisional_answer_stays_early() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer();
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "sendrecv", 1));
    offerer.apply_remote(&answer, false).expect("apply provisional");
    assert_eq!(offerer.state(), NegotiationState::Early);
}

// ---- negative: each maps to a typed rule ----

#[test]
fn glare_second_offer_is_rejected() {
    let mut e = engine("10.0.0.1", 40000, vec![PCMA]);
    e.local_offer(); // now OfferSent
    let offer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "sendrecv", 1));
    assert_eq!(e.answer_to(&offer).unwrap_err().rule, SdpRule::GlareSecondOffer);
}

#[test]
fn answer_codec_not_in_offer_is_rejected() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer();
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(0, "PCMU")], "sendrecv", 1));
    assert_eq!(
        offerer.apply_remote(&answer, true).unwrap_err().rule,
        SdpRule::AnswerCodecNotInOffer
    );
}

#[test]
fn empty_codec_intersection_is_rejected() {
    let mut answerer = engine("10.0.0.2", 40002, vec![PCMU]);
    let offer = parse_sdp(&audio_sdp("10.0.0.1", 40000, &[(8, "PCMA")], "sendrecv", 1));
    assert_eq!(
        answerer.answer_to(&offer).unwrap_err().rule,
        SdpRule::EmptyCodecIntersection
    );
}

#[test]
fn m_line_count_mismatch_is_rejected() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer(); // offer has 1 m-line
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "sendrecv", 2));
    assert_eq!(
        offerer.apply_remote(&answer, true).unwrap_err().rule,
        SdpRule::MLineCountMismatch
    );
}

#[test]
fn answer_without_offer_is_rejected() {
    let mut e = engine("10.0.0.1", 40000, vec![PCMA]);
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "sendrecv", 1));
    assert_eq!(
        e.apply_remote(&answer, true).unwrap_err().rule,
        SdpRule::AnswerWithoutOffer
    );
}

#[test]
fn no_media_offer_is_rejected() {
    let mut answerer = engine("10.0.0.2", 40002, vec![PCMA]);
    let offer = parse_sdp("v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\ns=-\r\nt=0 0\r\n");
    assert_eq!(answerer.answer_to(&offer).unwrap_err().rule, SdpRule::NoMedia);
}

// ---- direction / hold ----

#[test]
fn answer_port_zero_stops_send() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer();
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 0, &[(8, "PCMA")], "sendrecv", 1));
    let neg = offerer.apply_remote(&answer, true).expect("apply");
    assert!(!neg.send);
    assert_eq!(offerer.state(), NegotiationState::Held);
}

#[test]
fn answer_inactive_stops_both_directions() {
    let mut offerer = engine("10.0.0.1", 40000, vec![PCMA]);
    offerer.local_offer();
    let answer = parse_sdp(&audio_sdp("10.0.0.2", 40002, &[(8, "PCMA")], "inactive", 1));
    let neg = offerer.apply_remote(&answer, true).expect("apply");
    assert!(!neg.send && !neg.receive);
    assert_eq!(offerer.state(), NegotiationState::Held);
}

#[test]
fn answerer_reverses_sendonly_to_recvonly() {
    let mut answerer = engine("10.0.0.2", 40002, vec![PCMA]);
    let offer = parse_sdp(&audio_sdp("10.0.0.1", 40000, &[(8, "PCMA")], "sendonly", 1));
    answerer.answer_to(&offer).expect("answer");
    let neg = answerer.negotiated().unwrap();
    assert_eq!(neg.direction, MediaDirection::RecvOnly);
    assert!(!neg.send && neg.receive);
}

// ---- RFC 5009 P-Early-Media gate ----

#[test]
fn early_media_gate_truth_table() {
    assert!(is_early_media_authorized(true, Some(MediaDirection::SendRecv)));
    assert!(is_early_media_authorized(true, Some(MediaDirection::SendOnly)));
    assert!(!is_early_media_authorized(true, Some(MediaDirection::RecvOnly)));
    assert!(!is_early_media_authorized(true, Some(MediaDirection::Inactive)));
    assert!(!is_early_media_authorized(true, None));
    assert!(!is_early_media_authorized(false, Some(MediaDirection::SendRecv)));
}
