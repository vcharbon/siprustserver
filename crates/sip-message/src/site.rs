//! Where a byte of a raw datagram sits: its start line, the value of a named
//! header, or its body. A lenient lexical reading of the head (RFC 3261 §7:
//! start line, header lines up to the empty line, a line beginning with
//! whitespace continuing the header before it), for reporting where something
//! was found without a parse.

/// The part of a datagram a byte belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Site {
    StartLine,
    /// A header line, by the name as the datagram spells it.
    Header(String),
    Body,
}

/// Where byte `at` of `raw` sits.
pub fn site_at(raw: &[u8], at: usize) -> Site {
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map_or(raw.len(), |p| p + 2);
    if at >= head_end {
        return Site::Body;
    }
    let line_start =
        |end: usize| raw.iter().take(end).rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let mut start = line_start(at.min(raw.len()));
    while start > 0 && matches!(raw.get(start), Some(b' ' | b'\t')) {
        start = line_start(start - 1);
    }
    if start == 0 {
        return Site::StartLine;
    }
    // A line with no colon names no header: the empty name.
    let line = raw.get(start..).unwrap_or_default();
    let name = line.iter().position(|&b| b == b':').and_then(|c| line.get(..c)).unwrap_or_default();
    Site::Header(String::from_utf8_lossy(name).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_byte_sits_in_the_start_line_a_header_folded_or_not_or_the_body() {
        let raw = b"INVITE sip:a@h SIP/2.0\r\nSubject: one\r\n two\r\nCall-ID: c\r\n\r\nv=0\r\n";
        let at = |needle: &[u8]| raw.windows(needle.len()).position(|w| w == needle).unwrap();
        assert_eq!(site_at(raw, at(b"sip:a")), Site::StartLine);
        assert_eq!(site_at(raw, at(b"two")), Site::Header("Subject".into()));
        assert_eq!(site_at(raw, at(b": c")), Site::Header("Call-ID".into()));
        assert_eq!(site_at(raw, at(b"v=0")), Site::Body);
    }
}
