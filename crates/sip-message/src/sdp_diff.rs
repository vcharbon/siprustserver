//! Negotiated-media equivalence of two SDP bodies.
//!
//! [`sdp_media_equivalent`] returns true iff two descriptions steer media the
//! same way, stream by stream: each `m=` line's tokens, and the connection
//! address's tokens, bandwidths, key, direction and attributes the stream
//! takes, a
//! session-level value applying to every stream that states none of its own
//! (RFC 4566 §5). The origin, session name, timing and information lines,
//! line endings and attribute order make no difference. Pure: no clocks, no
//! I/O.

use crate::sdp_doc::{direction_of, parse_sdp_body, MediaLine, SdpDirection, SdpDoc};

/// What one stream of a description steers media with.
#[derive(PartialEq, Eq)]
struct Stream<'a> {
    media: (&'a str, Option<i64>, Option<i64>, &'a str, &'a [String]),
    /// The connection address's tokens (RFC 4566 §5.7).
    connection: Vec<&'a str>,
    bandwidths: Vec<&'a str>,
    key: Option<&'a str>,
    direction: SdpDirection,
    /// Every attribute but the direction, sorted.
    attributes: Vec<&'a str>,
}

/// An attribute's name: its text up to the first `:` (RFC 4566 §5.13).
fn attribute_name(attribute: &str) -> &str {
    attribute.split_once(':').map_or(attribute, |(name, _)| name)
}

/// `attributes` without their direction, each trimmed of trailing whitespace.
fn undirected(attributes: &[String]) -> impl Iterator<Item = &str> {
    attributes.iter().filter(|a| direction_of(a).is_none()).map(|a| crate::whitespace::trim_end(a))
}

/// `media` as it steers media under the session-level values of `doc`.
fn stream<'a>(doc: &'a SdpDoc, media: &'a MediaLine) -> Stream<'a> {
    let direction = media
        .attributes
        .iter()
        .chain(&doc.attributes)
        .find_map(|a| direction_of(a))
        .unwrap_or(SdpDirection::SendRecv);
    let own: Vec<&str> = undirected(&media.attributes).collect();
    let mut attributes = own.clone();
    attributes.extend(
        undirected(&doc.attributes)
            .filter(|a| !own.iter().any(|o| attribute_name(o) == attribute_name(a))),
    );
    attributes.sort_unstable();
    let stated = if media.bandwidths.is_empty() { &doc.bandwidths } else { &media.bandwidths };
    let mut bandwidths: Vec<&str> = stated.iter().map(|b| crate::whitespace::trim_end(b)).collect();
    bandwidths.sort_unstable();
    let connection = media.c_line.as_deref().or(doc.c_line.as_deref());
    Stream {
        media: (&media.r#type, media.port, media.port_count, &media.transport, &media.formats),
        connection: connection.map(|c| crate::whitespace::split(c).collect()).unwrap_or_default(),
        bandwidths,
        key: media.key.as_deref().or(doc.key.as_deref()).map(crate::whitespace::trim_end),
        direction,
        attributes,
    }
}

/// True iff `a` and `b` describe the same negotiated media session. Bodies
/// that are no session description are equivalent only when equal; empty
/// bodies only to each other.
pub fn sdp_media_equivalent(a: &[u8], b: &[u8]) -> bool {
    let (Some(doc_a), Some(doc_b)) = (parse_sdp_body(a), parse_sdp_body(b)) else {
        return a == b;
    };
    doc_a.media.len() == doc_b.media.len()
        && doc_a.media.iter().zip(&doc_b.media).all(|(x, y)| stream(&doc_a, x) == stream(&doc_b, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &[u8] = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    const A_DIFF_VERSION: &[u8] = b"v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=other\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=sendrecv\r\na=rtpmap:8 PCMA/8000\r\n";
    const A_DIFF_PORT: &[u8] = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    const A_LF_ONLY: &[u8] = b"v=0\no=alice 9 9 IN IP4 127.0.0.1\ns=-\nc=IN IP4 127.0.0.1\nt=0 0\nm=audio 10000 RTP/AVP 8\na=rtpmap:8 PCMA/8000\na=sendrecv\n";

    #[test]
    fn ignores_session_version_and_attr_order() {
        assert!(sdp_media_equivalent(A, A_DIFF_VERSION));
    }

    #[test]
    fn different_port_differs() {
        assert!(!sdp_media_equivalent(A, A_DIFF_PORT));
    }

    #[test]
    fn both_empty_equal_one_empty_differs() {
        assert!(sdp_media_equivalent(b"", b""));
        assert!(!sdp_media_equivalent(A, b""));
    }

    #[test]
    fn line_ending_style_is_not_a_difference() {
        assert!(sdp_media_equivalent(A, A_LF_ONLY));
    }

    #[test]
    fn media_level_attribute_differs() {
        let held = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=inactive\r\n";
        assert!(!sdp_media_equivalent(A, held));
    }

    /// RFC 4566 §5: a session-level value is every stream's default.
    #[test]
    fn a_moved_session_level_address_differs() {
        let moved = String::from_utf8_lossy(A).replace("c=IN IP4 127.0.0.1", "c=IN IP4 192.0.2.7");
        assert!(!sdp_media_equivalent(A, moved.as_bytes()));
    }

    #[test]
    fn one_address_at_either_level_is_the_same() {
        let media_level = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\nc=IN IP4 127.0.0.1\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
        assert!(sdp_media_equivalent(A, media_level));
    }

    #[test]
    fn a_session_level_direction_and_bandwidth_count() {
        let base = String::from_utf8_lossy(A).replace("a=sendrecv\r\n", "");
        let inactive = base.replace("t=0 0\r\n", "t=0 0\r\na=inactive\r\n");
        assert!(sdp_media_equivalent(A, base.as_bytes()), "no direction is sendrecv");
        assert!(!sdp_media_equivalent(base.as_bytes(), inactive.as_bytes()));
        let low = base.replace("t=0 0\r\n", "b=AS:64\r\nt=0 0\r\n");
        let high = base.replace("t=0 0\r\n", "b=AS:512\r\nt=0 0\r\n");
        assert!(!sdp_media_equivalent(low.as_bytes(), high.as_bytes()));
    }

    /// A media-level value overrides the session-level one of the same name.
    #[test]
    fn a_media_level_attribute_overrides_the_session_one() {
        let session = String::from_utf8_lossy(A).replace("t=0 0\r\n", "t=0 0\r\na=ptime:20\r\n");
        let both = format!("{session}a=ptime:30\r\n");
        let media = format!("{}a=ptime:30\r\n", String::from_utf8_lossy(A));
        assert!(sdp_media_equivalent(both.as_bytes(), media.as_bytes()));
    }

    /// A changed key, port count or byte that is no UTF-8 changes the media;
    /// spacing inside a connection line does not.
    #[test]
    fn tokens_and_octets_are_compared() {
        let base = String::from_utf8_lossy(A).into_owned();
        let k1 = format!("{base}k=clear:abc\r\n");
        let k2 = format!("{base}k=clear:xyz\r\n");
        assert!(!sdp_media_equivalent(k1.as_bytes(), k2.as_bytes()), "a changed k=");
        let two = base.replace("m=audio 10000 ", "m=audio 10000/2 ");
        let four = base.replace("m=audio 10000 ", "m=audio 10000/4 ");
        assert!(!sdp_media_equivalent(two.as_bytes(), four.as_bytes()), "a port count");
        let octet = |b: u8| [A, b"a=fmtp:8 x=", &[b], b"\r\n"].concat();
        assert!(!sdp_media_equivalent(&octet(0xe9), &octet(0xff)), "two bytes that are no UTF-8");
        assert!(sdp_media_equivalent(&octet(0xe9), &octet(0xe9)));
        let spaced = base.replace("c=IN IP4 127.0.0.1", "c=IN IP4  127.0.0.1");
        assert!(sdp_media_equivalent(A, spaced.as_bytes()), "a double space inside c=");
    }

    #[test]
    fn extra_m_section_differs() {
        let two = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\nm=video 10002 RTP/AVP 96\r\n";
        assert!(!sdp_media_equivalent(A, two));
    }
}
