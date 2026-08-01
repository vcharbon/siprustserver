//! Negotiated-media equivalence of two SDP bodies.
//!
//! [`sdp_media_equivalent`] returns true iff two bodies describe the same media
//! session for the purpose of deciding whether a B2BUA must resync its peer.
//! It compares only what steers media flow — the `m=` line tuples and the
//! `c=`/`b=`/`a=`/`i=`/`k=` lines attached to each, attribute order-insensitive
//! — and ignores session-level metadata (`o=` version, `s=`, `t=`, blank lines,
//! line endings). Pure and allocation-bounded: hot-path safe.

use crate::sdp::split_lines;

const MEDIA_LINE_PREFIX: &str = "m=";

/// One `m=` line and the media-level lines attached to it.
struct MediaBlock<'a> {
    m_line: &'a str,
    /// Sorted set of the block's c=/b=/a=/i=/k= lines.
    attributes: Vec<&'a str>,
}

/// Split into `m=`-rooted blocks. Lines before the first `m=` are session-level
/// and carry no media steering, so they are dropped.
fn media_blocks(text: &str) -> Vec<MediaBlock<'_>> {
    let mut blocks: Vec<MediaBlock<'_>> = Vec::new();

    for line in split_lines(text).into_iter().map(str::trim_end) {
        if line.starts_with(MEDIA_LINE_PREFIX) {
            blocks.push(MediaBlock { m_line: line, attributes: Vec::new() });
        } else if let Some(block) = blocks.last_mut() {
            if matches!(line.as_bytes().first(), Some(b'c' | b'b' | b'a' | b'i' | b'k')) {
                block.attributes.push(line);
            }
        }
    }

    for block in &mut blocks {
        block.attributes.sort_unstable();
    }
    blocks
}

/// True iff `a` and `b` describe the same negotiated media session. Empty
/// bodies are equivalent only to each other.
pub fn sdp_media_equivalent(a: &[u8], b: &[u8]) -> bool {
    if a.is_empty() || b.is_empty() {
        return a.is_empty() && b.is_empty();
    }
    let text_a = String::from_utf8_lossy(a);
    let text_b = String::from_utf8_lossy(b);
    let blocks_a = media_blocks(&text_a);
    let blocks_b = media_blocks(&text_b);

    blocks_a.len() == blocks_b.len()
        && blocks_a
            .iter()
            .zip(blocks_b.iter())
            .all(|(x, y)| x.m_line == y.m_line && x.attributes == y.attributes)
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

    #[test]
    fn extra_m_section_differs() {
        let two = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\nm=video 10002 RTP/AVP 96\r\n";
        assert!(!sdp_media_equivalent(A, two));
    }
}
