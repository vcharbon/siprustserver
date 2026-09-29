//! `answer_from_own` / `reject_offer`: an answer composed on a party's behalf
//! out of the description it stated earlier (RFC 3264 §6).
//!
//! The two worked examples follow one exchange from both sides: an early
//! re-offer answered as the first offerer would, and that first offer answered
//! from the re-offer.

use sip_message::{
    answer_direction, answer_from_own, reject_offer, BuildHeldSdpOptions, FormatPreference,
    SdpDirection,
};

/// The first offer: two audio streams, the first inactive, the second receive-only.
const FIRST_OFFER: &str = "v=0\r\no=alice 10 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 62986 RTP/AVP 0 4 18\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:4 G723/8000\r\na=rtpmap:18 G729/8000\r\na=inactive\r\nm=audio 51434 RTP/AVP 3 110\r\na=rtpmap:3 GSM/8000\r\na=rtpmap:110 telephone-event/8000\r\na=recvonly\r\n";
/// The re-offer: the two audio streams again and a video stream.
const REOFFER: &str = "v=0\r\no=bob 20 2 IN IP4 198.51.100.1\r\ns=-\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\nm=audio 99999 RTP/AVP 0 4\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:4 G723/8000\r\na=sendrecv\r\nm=audio 12345 RTP/AVP 3 110\r\na=rtpmap:3 GSM/8000\r\na=rtpmap:110 telephone-event/8000\r\na=sendonly\r\nm=video 53000 RTP/AVP 32\r\na=rtpmap:32 MPV/9000\r\n";

fn text(b: Option<Vec<u8>>) -> String {
    String::from_utf8(b.expect("an answer")).unwrap()
}

/// The re-offer answered out of the first offer, the re-offer ranking the
/// formats: one m-line per offered stream; the first keeps its one first
/// common format and stays inactive; the second keeps its format plus the
/// common telephone-event and receives; the video stream the first offer never
/// described is rejected. The origin is the one the caller names.
#[test]
fn a_reoffer_answered_out_of_the_first_offer() {
    let answer = answer_from_own(
        REOFFER.as_bytes(),
        FIRST_OFFER.as_bytes(),
        FormatPreference::Offerer,
        Some("alice 10 2 IN IP4 192.0.2.1"),
    );
    assert_eq!(
        text(answer),
        "v=0\r\no=alice 10 2 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 62986 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=inactive\r\nm=audio 51434 RTP/AVP 3 110\r\na=rtpmap:3 GSM/8000\r\na=rtpmap:110 telephone-event/8000\r\na=recvonly\r\nm=video 0 RTP/AVP 32\r\n",
    );
}

/// The first offer answered out of the re-offer, the re-offer ranking the
/// formats: one m-line per stream of the first offer (the video stream is not
/// there), the re-offer's ports and origin, the second stream sending only.
#[test]
fn the_first_offer_answered_out_of_the_reoffer() {
    let answer = answer_from_own(
        FIRST_OFFER.as_bytes(),
        REOFFER.as_bytes(),
        FormatPreference::Answerer,
        None,
    );
    assert_eq!(
        text(answer),
        "v=0\r\no=bob 20 2 IN IP4 198.51.100.1\r\ns=-\r\nc=IN IP4 198.51.100.1\r\nt=0 0\r\nm=audio 99999 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=inactive\r\nm=audio 12345 RTP/AVP 3 110\r\na=rtpmap:3 GSM/8000\r\na=rtpmap:110 telephone-event/8000\r\na=sendonly\r\n",
    );
}

/// First offer (G711, G729), re-offer (G729, G711): both answers keep G729,
/// the re-offerer's first choice.
#[test]
fn both_answers_keep_the_reofferers_first_choice() {
    let first = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8 18\r\n";
    let reoffer = "v=0\r\no=b 2 2 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 18 8\r\n";
    let to_reoffer = text(answer_from_own(
        reoffer.as_bytes(),
        first.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(to_reoffer.ends_with("m=audio 4000 RTP/AVP 18\r\n"), "{to_reoffer}");
    let to_first = text(answer_from_own(
        first.as_bytes(),
        reoffer.as_bytes(),
        FormatPreference::Answerer,
        None,
    ));
    assert!(to_first.ends_with("m=audio 5000 RTP/AVP 18\r\n"), "{to_first}");
    let offerer_order = text(answer_from_own(
        first.as_bytes(),
        reoffer.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(offerer_order.ends_with("m=audio 5000 RTP/AVP 8\r\n"), "{offerer_order}");
}

const DIRECTIONS: [SdpDirection; 4] = [
    SdpDirection::Inactive,
    SdpDirection::SendRecv,
    SdpDirection::SendOnly,
    SdpDirection::RecvOnly,
];

/// The table for the re-offer's answer: rows the first offer's direction,
/// columns the re-offer's, both in [`DIRECTIONS`] order.
#[test]
fn direction_answering_the_reoffer() {
    use SdpDirection::*;
    let table = [
        [Inactive, Inactive, Inactive, Inactive],
        [Inactive, SendRecv, RecvOnly, SendOnly],
        [Inactive, SendOnly, Inactive, SendOnly],
        [Inactive, RecvOnly, RecvOnly, Inactive],
    ];
    for (r, first) in DIRECTIONS.iter().enumerate() {
        for (c, re) in DIRECTIONS.iter().enumerate() {
            assert_eq!(answer_direction(*first, *re), table[r][c], "first {first:?}, re {re:?}");
        }
    }
}

/// The table for the first offer's answer: rows the first offer's direction,
/// columns the re-offer's.
#[test]
fn direction_answering_the_first_offer() {
    use SdpDirection::*;
    let table = [
        [Inactive, Inactive, Inactive, Inactive],
        [Inactive, SendRecv, SendOnly, RecvOnly],
        [Inactive, RecvOnly, Inactive, RecvOnly],
        [Inactive, SendOnly, SendOnly, Inactive],
    ];
    for (r, first) in DIRECTIONS.iter().enumerate() {
        for (c, re) in DIRECTIONS.iter().enumerate() {
            assert_eq!(answer_direction(*re, *first), table[r][c], "first {first:?}, re {re:?}");
        }
    }
}

/// Every direction pair through the builder: the answered stream states the
/// table's direction, written in place of its own or added where it differs
/// from the default it inherits.
#[test]
fn the_builder_writes_the_answered_direction() {
    let with_dir = |o: &str, v: u8, d: Option<SdpDirection>| {
        let attr = d.map(|d| format!("a={}\r\n", d.token())).unwrap_or_default();
        format!("v=0\r\no={o} 1 {v} IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\n{attr}")
    };
    for own in DIRECTIONS.iter().copied().map(Some).chain([None]) {
        for offered in DIRECTIONS.iter().copied().map(Some).chain([None]) {
            let answer = text(answer_from_own(
                with_dir("b", 2, offered).as_bytes(),
                with_dir("a", 1, own).as_bytes(),
                FormatPreference::Offerer,
                None,
            ));
            let expected = answer_direction(
                own.unwrap_or(SdpDirection::SendRecv),
                offered.unwrap_or(SdpDirection::SendRecv),
            );
            let stated = answer.lines().filter(|l| l.starts_with("a=")).collect::<Vec<_>>();
            match (own, expected) {
                (None, SdpDirection::SendRecv) => assert!(stated.is_empty(), "{answer}"),
                _ => assert_eq!(stated, [format!("a={}", expected.token())], "{answer}"),
            }
        }
    }
}

/// A session-level direction is what a stream without its own inherits; an
/// answered direction that differs overrides it at media level.
#[test]
fn a_session_level_direction_is_inherited() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\na=sendonly\r\nm=audio 4000 RTP/AVP 8\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\na=recvonly\r\n";
    let same =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert!(same.ends_with("a=sendonly\r\nm=audio 4000 RTP/AVP 8\r\n"), "{same}");
    let sending = offer.replace("a=recvonly", "a=sendonly");
    let off =
        text(answer_from_own(sending.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert!(off.ends_with("m=audio 4000 RTP/AVP 8\r\na=inactive\r\n"), "{off}");
}

/// No common format rejects the stream with the offer's m-line at port 0 and
/// no attribute; a stream the offer already rejects is answered with its own
/// line; a media type that differs at the same rank is rejected.
#[test]
fn rejected_streams() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\nm=audio 4002 RTP/AVP 0\r\nm=audio 4004 RTP/AVP 0\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 96\r\na=rtpmap:96 opus/48000/2\r\nm=audio 0 RTP/AVP 0 8\r\nm=video 5004 RTP/AVP 31\r\n";
    assert_eq!(
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None)),
        "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\nm=audio 0 RTP/AVP 0 8\r\nm=video 0 RTP/AVP 31\r\n",
    );
}

/// A stream the own description rejects stays rejected with its own m-line.
#[test]
fn a_stream_the_own_description_rejects_stays_rejected() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 8 0\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Answerer, None));
    assert!(answer.ends_with("t=0 0\r\nm=audio 0 RTP/AVP 8 0\r\n"), "{answer}");
}

/// The own description's `rtpmap` and `fmtp` lines ride only for the formats
/// kept; its other attributes and media-level lines ride as written.
#[test]
fn only_the_kept_formats_keep_their_attributes() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8 18 101\r\nc=IN IP4 192.0.2.1\r\nb=AS:64\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=no\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-15\r\na=ptime:20\r\na=sqn:0\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 18 101\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=yes\r\na=rtpmap:101 telephone-event/8000\r\n";
    assert_eq!(
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None)),
        "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 18 101\r\nc=IN IP4 192.0.2.1\r\nb=AS:64\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=no\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-15\r\na=ptime:20\r\na=sqn:0\r\n",
    );
}

/// `telephone-event` is kept only beside a media format, only on audio, and
/// only when both ends list it.
#[test]
fn telephone_event_rides_beside_a_common_format() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8 101\r\na=rtpmap:101 telephone-event/8000\r\n";
    let only_events = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 101 0\r\na=rtpmap:101 telephone-event/8000\r\n";
    let answer = text(answer_from_own(
        only_events.as_bytes(),
        own.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(
        answer.ends_with("t=0 0\r\nm=audio 0 RTP/AVP 101 0\r\n"),
        "events alone carry no media: {answer}"
    );
    let no_events = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\n";
    let answer = text(answer_from_own(
        no_events.as_bytes(),
        own.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(answer.ends_with("m=audio 4000 RTP/AVP 8\r\n"), "no common events: {answer}");
}

/// A payload type both ends list but bind to different encodings is not common.
#[test]
fn a_payload_type_bound_differently_is_not_common() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96 8\r\na=rtpmap:96 AMR/8000\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 96 8\r\na=rtpmap:96 opus/48000/2\r\na=rtpmap:8 pcma/8000/1\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert!(answer.ends_with("m=audio 4000 RTP/AVP 8\r\n"), "{answer}");
}

/// A rejected stream carries the own description's `c=` where it states none
/// at session level (RFC 4566 §5.7).
#[test]
fn a_rejected_stream_carries_a_connection_where_the_session_has_none() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\nc=IN IP4 192.0.2.1\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\nm=video 5002 RTP/AVP 31\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert!(answer.ends_with("m=video 0 RTP/AVP 31\r\nc=IN IP4 192.0.2.1\r\n"), "{answer}");
}

#[test]
fn nothing_is_built_from_what_is_not_a_description() {
    assert_eq!(answer_from_own(b"", FIRST_OFFER.as_bytes(), FormatPreference::Offerer, None), None);
    assert_eq!(answer_from_own(REOFFER.as_bytes(), b"x", FormatPreference::Offerer, None), None);
    assert_eq!(reject_offer(b"not sdp", &at("192.0.2.1")), None);
}

fn at(ip: &str) -> BuildHeldSdpOptions {
    BuildHeldSdpOptions { local_ip: ip.into(), now_ms: 7_000 }
}

/// Every offered stream rejected, in order, with its formats and inactive,
/// under the answerer's own origin and address; one already rejected is
/// answered with its own line.
#[test]
fn an_offer_rejected_whole() {
    assert_eq!(
        text(reject_offer(REOFFER.replace("12345", "0").as_bytes(), &at("192.0.2.9"))),
        "v=0\r\no=b2bua 7 7 IN IP4 192.0.2.9\r\ns=-\r\nc=IN IP4 192.0.2.9\r\nt=0 0\r\nm=audio 0 RTP/AVP 0 4\r\na=inactive\r\nm=audio 0 RTP/AVP 3 110\r\na=inactive\r\nm=video 0 RTP/AVP 32\r\na=inactive\r\n",
    );
    let v6 = text(reject_offer(REOFFER.as_bytes(), &at("::1")));
    assert!(v6.contains("o=b2bua 7 7 IN IP6 ::1\r\n") && v6.contains("c=IN IP6 ::1\r\n"), "{v6}");
}

fn stream_of(answer: &str) -> String {
    answer[answer.find("m=").expect("a stream")..].to_string()
}

/// A dynamic format is the same codec under another number when its encoding
/// (name, clock, channels) is (RFC 3264 §6.1): the answer uses the OFFERER's
/// number and rtpmap, telephone-event included; the answerer's fmtp follows
/// the renumbered format.
#[test]
fn dynamic_formats_match_by_encoding_under_the_offerers_number() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96 101\r\na=rtpmap:96 AMR-WB/16000\r\na=fmtp:96 mode-change-capability=2\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-15\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 97 100\r\na=rtpmap:97 amr-wb/16000/1\r\na=rtpmap:100 telephone-event/8000\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=audio 4000 RTP/AVP 97 100\r\na=rtpmap:97 amr-wb/16000/1\r\na=fmtp:97 mode-change-capability=2\r\na=rtpmap:100 telephone-event/8000\r\na=fmtp:100 0-15\r\n",
    );
    let other_clock = offer.replace("telephone-event/8000", "telephone-event/16000");
    let answer = text(answer_from_own(
        other_clock.as_bytes(),
        own.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(stream_of(&answer).starts_with("m=audio 4000 RTP/AVP 97\r\n"), "{answer}");
}

/// Per-format attributes ride only for the formats kept, whatever the
/// attribute (RFC 4585 rtcp-fb included); a wildcard one rides as written.
#[test]
fn every_per_format_attribute_follows_its_format() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=video 4000 RTP/AVPF 96 97\r\na=rtpmap:96 H264/90000\r\na=rtpmap:97 VP8/90000\r\na=rtcp-fb:96 nack\r\na=rtcp-fb:97 nack pli\r\na=rtcp-fb:* ccm fir\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=video 5000 RTP/AVPF 97\r\na=rtpmap:97 VP8/90000\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=video 4000 RTP/AVPF 97\r\na=rtpmap:97 VP8/90000\r\na=rtcp-fb:97 nack pli\r\na=rtcp-fb:* ccm fir\r\n",
    );
}

/// A stream offered under another transport profile is rejected: an RTP/AVP
/// answer to an RTP/SAVP offer is no answer (RFC 3264 §6).
#[test]
fn another_transport_profile_rejects_the_stream() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/SAVP 8\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIz\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(stream_of(&answer), "m=audio 0 RTP/SAVP 8\r\n");
}

/// SDES (RFC 4568 §5.1.2): the answer carries ONE crypto line, the answerer's
/// key for the first offered suite it holds, under the OFFER's tag; no suite
/// in common rejects the stream.
#[test]
fn an_sdes_offer_is_answered_with_one_crypto_line_under_the_offers_tag() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/SAVP 8\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:AAAA\r\na=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:BBBB\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/SAVP 8\r\na=crypto:5 AES_CM_128_HMAC_SHA1_32 inline:CCCC\r\na=crypto:6 AES_CM_128_HMAC_SHA1_80 inline:DDDD\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=audio 4000 RTP/SAVP 8\r\na=crypto:5 AES_CM_128_HMAC_SHA1_32 inline:BBBB\r\n"
    );
    let none = offer
        .replace("AES_CM_128_HMAC_SHA1_32", "F8_128_HMAC_SHA1_80")
        .replace("crypto:6 AES_CM_128_HMAC_SHA1_80", "crypto:6 AEAD_AES_256_GCM");
    let answer =
        text(answer_from_own(none.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(stream_of(&answer), "m=audio 0 RTP/SAVP 8\r\n");
}

/// AMR / AMR-WB in octet-aligned and bandwidth-efficient mode are two payload
/// formats (RFC 4867 §8.3.1): not common.
#[test]
fn amr_octet_alignment_must_agree() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 96 8\r\na=rtpmap:96 AMR/8000\r\na=fmtp:96 octet-align=1\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 98 8\r\na=rtpmap:98 AMR/8000\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(stream_of(&answer), "m=audio 4000 RTP/AVP 8\r\n");
    let aligned =
        offer.replace("AMR/8000\r\n", "AMR/8000\r\na=fmtp:98 octet-align=1; mode-set=0,2\r\n");
    let answer =
        text(answer_from_own(aligned.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=audio 4000 RTP/AVP 98\r\na=rtpmap:98 AMR/8000\r\na=fmtp:98 octet-align=1\r\n"
    );
}

/// RFC 5888 §9.2: each answered m-line carries the offer's `mid`; the own
/// description's session-level `group` (here lip-sync) is not an answer to
/// the offer's and is not copied.
#[test]
fn the_answer_takes_the_offers_mid_and_states_no_group_of_its_own() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\na=group:LS 0\r\nm=audio 4000 RTP/AVP 8\r\na=mid:0\r\na=ptime:20\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\na=mid:voice\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        answer,
        "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\na=mid:voice\r\na=ptime:20\r\n",
    );
    let no_mid = offer.replace("a=mid:voice\r\n", "");
    let answer =
        text(answer_from_own(no_mid.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert!(stream_of(&answer) == "m=audio 4000 RTP/AVP 8\r\na=ptime:20\r\n", "{answer}");
    let own_no_mid = own.replace("a=mid:0\r\n", "");
    let answer = text(answer_from_own(
        offer.as_bytes(),
        own_no_mid.as_bytes(),
        FormatPreference::Offerer,
        None,
    ));
    assert!(
        stream_of(&answer) == "m=audio 4000 RTP/AVP 8\r\na=ptime:20\r\na=mid:voice\r\n",
        "{answer}"
    );
}

/// DTLS-SRTP (RFC 5763) is out of scope: the handshake role and fingerprint
/// are the party's own, so a stream either side sets up over DTLS is rejected.
#[test]
fn a_dtls_stream_is_rejected() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 UDP/TLS/RTP/SAVP 8\r\na=setup:actpass\r\na=fingerprint:sha-256 AB:CD\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 UDP/TLS/RTP/SAVP 8\r\na=setup:actpass\r\na=fingerprint:sha-256 EF:01\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(stream_of(&answer), "m=audio 0 UDP/TLS/RTP/SAVP 8\r\n");
}

/// A kept dynamic number the own description binds with no rtpmap (a static
/// format under the offer's dynamic number) carries the offer's rtpmap (RFC
/// 4566 §6: a dynamic payload type is always mapped).
#[test]
fn a_dynamic_number_always_carries_its_rtpmap() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=ptime:20\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 97\r\na=rtpmap:97 PCMU/8000\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=audio 4000 RTP/AVP 97\r\na=ptime:20\r\na=rtpmap:97 PCMU/8000\r\n"
    );
}

/// SDES either side carries and the other does not is no suite in common
/// (RFC 4568 §5.1.2): the stream is rejected whichever side holds the keys.
#[test]
fn keys_on_one_side_only_reject_the_stream() {
    let keyed = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:AAAA\r\n";
    let plain = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\n";
    for (offer, own) in [(plain, keyed), (keyed, plain)] {
        let answer = text(answer_from_own(
            offer.as_bytes(),
            own.as_bytes(),
            FormatPreference::Offerer,
            None,
        ));
        assert!(stream_of(&answer).starts_with("m=audio 0 RTP/AVP 8\r\n"), "{answer}");
    }
}

/// The two answers of one exchange accept the same streams: a stream the
/// agreed answer rejects at a rank is rejected here too.
#[test]
fn a_stream_the_agreed_answer_rejects_is_rejected() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\nm=video 4002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 8\r\nm=video 5002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
    let agreed = "v=0\r\no=b 1 2 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 0 RTP/AVP 8\r\nm=video 5002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
    let answer = text(sip_message::answer_from_own_agreeing(
        offer.as_bytes(),
        own.as_bytes(),
        FormatPreference::Offerer,
        None,
        Some(agreed.as_bytes()),
    ));
    assert_eq!(
        stream_of(&answer),
        "m=audio 0 RTP/AVP 8\r\nm=video 4002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"
    );
}

/// BUNDLE (RFC 8843) is out of scope: an offer or own description grouping
/// streams under it cannot be answered on a party's behalf.
#[test]
fn a_bundle_group_is_not_answered() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\n";
    let bundled = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\na=group:BUNDLE a v\r\nm=audio 5000 RTP/AVP 8\r\na=mid:a\r\nm=video 5000 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=mid:v\r\n";
    assert_eq!(
        answer_from_own(bundled.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None),
        None
    );
    assert_eq!(
        answer_from_own(own.as_bytes(), bundled.as_bytes(), FormatPreference::Answerer, None),
        None
    );
}

/// A rejected m-line keeps the offer's `mid` (RFC 5888 §9.2).
#[test]
fn a_rejected_stream_keeps_the_offers_mid() {
    let own = "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 8\r\n";
    let offer = "v=0\r\no=b 1 1 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 5000 RTP/AVP 0\r\na=mid:1\r\nm=video 5002 RTP/AVP 31\r\na=mid:2\r\n";
    let answer =
        text(answer_from_own(offer.as_bytes(), own.as_bytes(), FormatPreference::Offerer, None));
    assert_eq!(
        stream_of(&answer),
        "m=audio 0 RTP/AVP 0\r\na=mid:1\r\nm=video 0 RTP/AVP 31\r\na=mid:2\r\n"
    );
}
