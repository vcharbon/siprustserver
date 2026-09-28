//! `answer_from_own` / `reject_offer`: an answer composed on a party's behalf
//! out of the description it stated earlier (RFC 3264 §6).
//!
//! The two worked examples follow one exchange from both sides: an early
//! re-offer answered as the first offerer would, and that first offer answered
//! from the re-offer.

use sip_message::{
    answer_direction, answer_from_own, reject_offer, FormatPreference, SdpDirection,
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
    assert_eq!(reject_offer(b"not sdp", "a 1 1 IN IP4 192.0.2.1", "192.0.2.1"), None);
}

/// Every offered stream rejected, in order, with its formats and inactive;
/// one already rejected is answered with its own line.
#[test]
fn an_offer_rejected_whole() {
    assert_eq!(
        text(reject_offer(REOFFER.replace("12345", "0").as_bytes(), "as 7 7 IN IP4 192.0.2.9", "192.0.2.9")),
        "v=0\r\no=as 7 7 IN IP4 192.0.2.9\r\ns=-\r\nc=IN IP4 192.0.2.9\r\nt=0 0\r\nm=audio 0 RTP/AVP 0 4\r\na=inactive\r\nm=audio 0 RTP/AVP 3 110\r\na=inactive\r\nm=video 0 RTP/AVP 32\r\na=inactive\r\n",
    );
    let v6 = text(reject_offer(REOFFER.as_bytes(), "as 7 7 IN IP6 ::1", "::1"));
    assert!(v6.contains("c=IN IP6 ::1\r\n"), "{v6}");
}
