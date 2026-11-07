//! The head of one MIME part (RFC 2045 §3, RFC 2046 §5.1): where it ends,
//! and its entity headers read by name, listed, and located.
//!
//! The head ends at the first empty line counted from the block's start: a
//! block starting with one has no headers, and a block holding none is all
//! headers and no content. A line starting with SP or HTAB continues the
//! header before it, and a value reads unfolded. Line ends are CRLF only, and
//! the whitespace around a name and a value is SP and HTAB.

use std::ops::Range;

/// One entity header of a part's head.
#[derive(Debug, Clone)]
struct Field {
    name: String,
    /// The value unfolded, its surrounding SP and HTAB left out.
    value: String,
    /// The value's range in the body, folds included.
    span: Range<usize>,
}

/// The entity headers of a part's head, in wire order.
#[derive(Debug, Clone, Default)]
pub(super) struct PartHead {
    fields: Vec<Field>,
}

fn is_wsp(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

fn trim_wsp(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| !is_wsp(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|&b| !is_wsp(b)).map_or(start, |i| i + 1);
    bytes.get(start..end.max(start)).unwrap_or_default()
}

/// The CRLF-ended lines of `bytes[range]`, each as its range in `bytes`, the
/// CRLF left out; the last line ends with the range.
fn lines(bytes: &[u8], range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = range.start;
    let text = bytes.get(range.clone()).unwrap_or_default();
    for found in memchr::memmem::find_iter(text, b"\r\n") {
        let end = range.start + found;
        out.push(at..end);
        at = end + 2;
    }
    out.push(at..range.end);
    out
}

impl PartHead {
    /// The head and the content of the part whose block is `block` of `body`;
    /// `None` when `block` is no range of `body`.
    pub(super) fn split(body: &[u8], block: Range<usize>) -> Option<(PartHead, Range<usize>)> {
        let bytes = body.get(block.clone())?;
        if bytes.starts_with(b"\r\n") {
            return Some((PartHead::default(), block.start + 2..block.end));
        }
        let (head, content) = match memchr::memmem::find(bytes, b"\r\n\r\n") {
            Some(i) => (block.start..block.start + i, block.start + i + 4..block.end),
            None => (block.clone(), block.end..block.end),
        };
        Some((PartHead::read(body, head), content))
    }

    /// The fields of the head lying at `head` of `body`. A line holding no
    /// colon, and a continuation with no header before it, is no field.
    fn read(body: &[u8], head: Range<usize>) -> PartHead {
        let mut fields: Vec<Field> = Vec::new();
        let mut open: Option<(String, Range<usize>)> = None;
        for line in lines(body, head) {
            let text = body.get(line.clone()).unwrap_or_default();
            if text.first().copied().is_some_and(is_wsp) {
                if let Some((_, value)) = open.as_mut() {
                    value.end = line.end;
                }
                continue;
            }
            fields.extend(open.take().map(|(name, value)| field(body, name, value)));
            let Some(colon) = text.iter().position(|&b| b == b':') else { continue };
            let name = trim_wsp(text.get(..colon).unwrap_or_default());
            if !name.is_empty() {
                let name = String::from_utf8_lossy(name).into_owned();
                open = Some((name, line.start + colon + 1..line.end));
            }
        }
        fields.extend(open.take().map(|(name, value)| field(body, name, value)));
        PartHead { fields }
    }

    /// The first value of `name`, case aside.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        let field = self.fields.iter().find(|f| f.name.eq_ignore_ascii_case(name))?;
        Some(field.value.as_str())
    }

    /// Every header but those named in `own_fields`, `(name, value)`, in wire
    /// order and with the part's own spelling.
    pub(super) fn beyond(&self, own_fields: &[&str]) -> Vec<(String, String)> {
        let own = |name: &str| own_fields.iter().any(|own| name.eq_ignore_ascii_case(own));
        let kept = self.fields.iter().filter(|f| !own(&f.name));
        kept.map(|f| (f.name.clone(), f.value.clone())).collect()
    }

    /// Every header as its name and the range of its value in the body, in
    /// wire order.
    pub(super) fn spans(&self) -> Vec<(String, Range<usize>)> {
        self.fields.iter().map(|f| (f.name.clone(), f.span.clone())).collect()
    }
}

/// The field `name` whose raw value, folds included, lies at `raw` of `body`.
fn field(body: &[u8], name: String, raw: Range<usize>) -> Field {
    let bytes = body.get(raw.clone()).unwrap_or_default();
    let lead = bytes.iter().take_while(|&&b| is_wsp(b) || b == b'\r' || b == b'\n').count();
    let trail = bytes.iter().rev().take_while(|&&b| is_wsp(b)).count();
    let start = raw.start + lead;
    let span = start..(raw.end - trail).max(start);
    let unfolded: Vec<u8> = {
        let mut out = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        while let Some(i) = memchr::memmem::find(rest, b"\r\n") {
            out.extend_from_slice(rest.get(..i).unwrap_or_default());
            rest = rest.get(i + 2..).unwrap_or_default();
        }
        out.extend_from_slice(rest);
        out
    };
    let value = String::from_utf8_lossy(trim_wsp(&unfolded)).into_owned();
    Field { name, value, span }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(block: &str) -> (PartHead, &str) {
        let (head, content) = PartHead::split(block.as_bytes(), 0..block.len()).unwrap();
        (head, &block[content])
    }

    #[test]
    fn the_head_ends_at_the_first_empty_line() {
        let (head, content) = split("Content-Type: text/plain\r\nX-A: 1\r\n\r\nbody\r\n\r\nmore");
        assert_eq!(head.get("content-type"), Some("text/plain"));
        assert_eq!(head.get("x-a"), Some("1"));
        assert_eq!(content, "body\r\n\r\nmore");
    }

    #[test]
    fn a_block_starting_with_an_empty_line_has_no_headers() {
        let (head, content) = split("\r\nContent-Type: application/sdp\r\n\r\nv=0");
        assert!(head.fields.is_empty());
        assert_eq!(content, "Content-Type: application/sdp\r\n\r\nv=0");
    }

    #[test]
    fn a_block_with_no_empty_line_is_all_headers() {
        let (head, content) = split("Content-Type: application/x\r\nContent-ID: <p1>");
        assert_eq!(head.get("Content-Type"), Some("application/x"));
        assert_eq!(head.get("Content-ID"), Some("<p1>"));
        assert_eq!(content, "");
    }

    #[test]
    fn a_continuation_belongs_to_the_header_before_it() {
        let block =
            "Content-Type:\r\n application/sdp\r\nX-Note: first\r\n Content-Type: x\r\n\r\n";
        let (head, _) = split(block);
        assert_eq!(head.get("content-type"), Some("application/sdp"));
        assert_eq!(head.get("x-note"), Some("first Content-Type: x"));
        assert_eq!(head.beyond(&["content-type"]).len(), 1, "the continuation is no header");
        let spans = head.spans();
        assert_eq!(&block[spans[0].1.clone()], "application/sdp");
        assert_eq!(&block[spans[1].1.clone()], "first\r\n Content-Type: x");
    }

    #[test]
    fn whitespace_is_sp_and_htab_only() {
        let (head, _) = split("\u{b}Content-Type: application/sdp\r\nX-A:\u{a0}1\u{a0}\r\n\r\n");
        assert_eq!(head.get("content-type"), None, "a VT is part of the name");
        assert_eq!(head.get("x-a"), Some("\u{a0}1\u{a0}"));
    }
}
