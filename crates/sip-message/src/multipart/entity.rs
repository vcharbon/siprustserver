//! The entity headers of one MIME part's header block (RFC 2045 §3): read by
//! name, listed beyond the ones held in their own field, and located.

/// One header line split at its first colon into name and value; `None` for a
/// line holding no colon.
fn field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut halves = line.splitn(2, |&b| b == b':');
    Some((halves.next()?, halves.next()?))
}

/// Every entity header of a part's header block except the ones already held in
/// their own field, in wire order and with the part's own spelling.
pub(super) fn part_headers_beyond(head: &[u8], own_fields: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in head.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some((name, value)) = field(line) else { continue };
        let name = String::from_utf8_lossy(name).trim().to_string();
        if name.is_empty() || own_fields.iter().any(|own| name.eq_ignore_ascii_case(own)) {
            continue;
        }
        out.push((name, String::from_utf8_lossy(value).trim().to_string()));
    }
    out
}

/// Every entity header of a part's header block as its name and the range of
/// its value, trimmed, in the body the block starts at `at` of.
pub(super) fn part_header_spans(head: &[u8], at: usize) -> Vec<(String, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut line_at = 0;
    for line in head.split(|&b| b == b'\n') {
        let next = line_at + line.len() + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some((name, value)) = field(line) {
            let colon = name.len();
            let name = String::from_utf8_lossy(name).trim().to_string();
            let lead = value.iter().take_while(|b| b.is_ascii_whitespace()).count();
            let trail = value.iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
            let from = at + line_at + colon + 1 + lead;
            let to = (at + line_at + colon + 1 + value.len() - trail).max(from);
            if !name.is_empty() {
                out.push((name, from..to));
            }
        }
        line_at = next;
    }
    out
}

/// One MIME entity header of a part's header block, case-insensitively.
pub(super) fn part_header(head: &[u8], name_lower: &str) -> Option<String> {
    for line in head.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some((name, value)) = field(line) else { continue };
        if String::from_utf8_lossy(name).trim().eq_ignore_ascii_case(name_lower) {
            return Some(String::from_utf8_lossy(value).trim().to_string());
        }
    }
    None
}
