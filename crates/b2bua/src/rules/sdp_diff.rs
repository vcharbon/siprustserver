//! Negotiated-media SDP comparison — port of
//! `src/b2bua/rules/custom/_shared/sdpDiff.ts`.
//!
//! [`sdp_media_equivalent`] returns true iff two SDP bodies describe the same
//! media session for the purpose of deciding whether the B2BUA must re-INVITE
//! Alice. It compares only what steers media flow (m= line tuples + their
//! attached `c=`/`b=`/`a=`/`i=`/`k=` lines, attribute order-insensitive) and
//! ignores session-level metadata (`o=` version, `s=`, `t=`, blank lines, line
//! endings). Pure — hot-path safe in a rule handler.

const MEDIA_LINE_PREFIX: &str = "m=";

struct MediaBlock {
    m_line: String,
    /// Sorted set of c=/b=/a=/i=/k= lines under this m= block.
    attributes: Vec<String>,
}

fn split_lines(body: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(body)
        .split(['\r', '\n'])
        .map(|l| l.trim_end().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn parse_media_blocks(body: &[u8]) -> Vec<MediaBlock> {
    let mut blocks: Vec<MediaBlock> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    for line in split_lines(body) {
        if line.starts_with(MEDIA_LINE_PREFIX) {
            if let Some((m, mut attrs)) = current.take() {
                attrs.sort();
                blocks.push(MediaBlock { m_line: m, attributes: attrs });
            }
            current = Some((line, Vec::new()));
        } else if let Some((_, attrs)) = current.as_mut() {
            match line.chars().next() {
                Some('c') | Some('b') | Some('a') | Some('i') | Some('k') => attrs.push(line),
                _ => {}
            }
        }
        // Session-level lines before any m= are ignored.
    }
    if let Some((m, mut attrs)) = current.take() {
        attrs.sort();
        blocks.push(MediaBlock { m_line: m, attributes: attrs });
    }
    blocks
}

/// True iff `a` and `b` represent the same negotiated media session. Empty
/// bodies are equal only when both are empty.
pub fn sdp_media_equivalent(a: &[u8], b: &[u8]) -> bool {
    if a.is_empty() && b.is_empty() {
        return true;
    }
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let ba = parse_media_blocks(a);
    let bb = parse_media_blocks(b);
    if ba.len() != bb.len() {
        return false;
    }
    ba.iter().zip(bb.iter()).all(|(x, y)| {
        x.m_line == y.m_line && x.attributes == y.attributes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &[u8] = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    const A_DIFF_VERSION: &[u8] = b"v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=other\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=sendrecv\r\na=rtpmap:8 PCMA/8000\r\n";
    const A_DIFF_PORT: &[u8] = b"v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

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
}
