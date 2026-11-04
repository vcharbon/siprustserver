//! The parts of a framed body as they lie in it (RFC 2046 §5.1): located, not
//! copied.

/// One part of a framed body as [`framed_parts`] finds it: its entity header
/// block and where its content lies.
pub(super) struct FramedPart<'a> {
    pub(super) head: &'a [u8],
    /// Where the header block starts in the body.
    pub(super) head_at: usize,
    /// Where the content starts in the body.
    pub(super) offset: usize,
    /// The content's length in bytes.
    pub(super) len: usize,
}

/// The parts of `body` framed under `boundary` (RFC 2046 §5.1), in order: the
/// preamble and the closing delimiter dropped, each block split into its
/// entity headers and its content, nothing copied.
pub(super) fn framed_parts<'a>(
    body: &'a [u8],
    boundary: &str,
) -> impl Iterator<Item = FramedPart<'a>> {
    let delim = format!("--{boundary}");
    segments(body, delim.as_bytes()).into_iter().skip(1).filter_map(|(start, block)| {
        // A closing delimiter's remainder begins with `--`.
        if block.starts_with(b"--") {
            return None;
        }
        // The CRLF that ended the boundary line belongs to the delimiter.
        let lead = if block.starts_with(b"\r\n") {
            2
        } else if block.starts_with(b"\n") {
            1
        } else {
            0
        };
        let block = block.get(lead..).filter(|b| !b.is_empty())?;
        let (head_len, gap) = match find(block, b"\r\n\r\n") {
            Some(i) => (i, 4),
            None => match find(block, b"\n\n") {
                Some(i) => (i, 2),
                None => (0, 0),
            },
        };
        let (head, content) = (block.get(..head_len)?, block.get(head_len + gap..)?);
        // Exactly the ONE trailing CRLF that separates the content from the
        // next delimiter — never greedily, so a binary part ending in 0x0a
        // survives byte-exact.
        let trailing = if content.ends_with(b"\r\n") {
            2
        } else if content.ends_with(b"\n") {
            1
        } else {
            0
        };
        Some(FramedPart {
            head,
            head_at: start + lead,
            offset: start + lead + head_len + gap,
            len: content.len() - trailing,
        })
    })
}

/// `(offset, slice)` for every stretch of `hay` between occurrences of `sep`,
/// the separators dropped. The first entry is whatever preceded the first
/// separator, so a caller wanting the parts skips it.
fn segments<'a>(hay: &'a [u8], sep: &[u8]) -> Vec<(usize, &'a [u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut rest = hay;
    while let Some((rel, before, after)) =
        find(rest, sep).and_then(|rel| Some((rel, rest.get(..rel)?, rest.get(rel + sep.len()..)?)))
    {
        out.push((at, before));
        at += rel + sep.len();
        rest = after;
    }
    out.push((at, rest));
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    memchr::memmem::find(hay, needle)
}
