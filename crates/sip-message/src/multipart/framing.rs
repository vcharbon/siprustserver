//! The parts of a framed body as they lie in it (RFC 2046 §5.1.1): located,
//! not copied.
//!
//! A delimiter is a line: `--boundary` at the body's start or right after a
//! CRLF, then either `--` (the close delimiter) or optional transport padding
//! (SP / HTAB) and a CRLF or the body's end. The CRLF before a delimiter
//! belongs to it, never to the part it ends. Line ends are CRLF only. Reading
//! stops at the close delimiter, and a body that never closes keeps its last
//! part, up to the body's end.

use super::entity::PartHead;

/// One part of a framed body as [`framed_parts`] finds it: its head read, and
/// where its content lies.
pub(super) struct FramedPart {
    pub(super) head: PartHead,
    /// Where the content starts in the body.
    pub(super) offset: usize,
    /// The content's length in bytes.
    pub(super) len: usize,
}

/// The parts of `body` framed under `boundary`, in order: the preamble and
/// the epilogue dropped, each block split into its head and its content
/// ([`PartHead::split`]). An empty boundary frames nothing.
pub(super) fn framed_parts(body: &[u8], boundary: &str) -> Vec<FramedPart> {
    let blocks = blocks(body, boundary.as_bytes());
    blocks
        .into_iter()
        .filter_map(|block| {
            let (head, content) = PartHead::split(body, block)?;
            Some(FramedPart { head, offset: content.start, len: content.len() })
        })
        .collect()
}

/// What follows `--boundary` on its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    /// An encapsulation boundary; the next part starts at this offset.
    Open { next: usize },
    /// The close delimiter.
    Close,
}

/// The delimiter `--boundary` at `at` of `body` would be, `None` when the
/// rest of its line makes it none.
fn delimiter_at(body: &[u8], at: usize, dash_boundary_len: usize) -> Option<Delimiter> {
    let rest = body.get(at + dash_boundary_len..)?;
    if rest.starts_with(b"--") {
        return Some(Delimiter::Close);
    }
    let padding = rest.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
    let after = rest.get(padding..)?;
    let end = at + dash_boundary_len + padding;
    if after.is_empty() {
        Some(Delimiter::Open { next: end })
    } else if after.starts_with(b"\r\n") {
        Some(Delimiter::Open { next: end + 2 })
    } else {
        None
    }
}

/// The next delimiter of `body` at or after `from`: where its line starts and
/// what it is. A line counts when it starts at the body's start or right after
/// a CRLF at or after `crlf_from`, so a CRLF ends one line only.
fn next_delimiter(
    body: &[u8],
    dash_boundary: &[u8],
    from: usize,
    crlf_from: usize,
) -> Option<(usize, Delimiter)> {
    let mut search = from;
    loop {
        let at = search + memchr::memmem::find(body.get(search..)?, dash_boundary)?;
        let starts_a_line =
            at == 0 || (at >= crlf_from + 2 && body.get(at - 2..at) == Some(b"\r\n".as_slice()));
        if starts_a_line {
            if let Some(delimiter) = delimiter_at(body, at, dash_boundary.len()) {
                return Some((at, delimiter));
            }
        }
        search = at + 1;
    }
}

/// The delimiters of `body` under `boundary`: where each line starts, the
/// close delimiter last when the body has one.
fn delimiters(body: &[u8], dash_boundary: &[u8]) -> Vec<(usize, Delimiter)> {
    let mut out = Vec::new();
    let Some(first) = next_delimiter(body, dash_boundary, 0, 0) else {
        return out;
    };
    out.push(first);
    let mut delimiter = first.1;
    while let Delimiter::Open { next: start } = delimiter {
        let Some(found) = next_delimiter(body, dash_boundary, start, start) else { break };
        out.push(found);
        delimiter = found.1;
    }
    out
}

/// The byte range of every part's block: from the end of its delimiter line
/// to the CRLF of the next delimiter, or to the body's end when none follows.
fn blocks(body: &[u8], boundary: &[u8]) -> Vec<std::ops::Range<usize>> {
    if boundary.is_empty() {
        return Vec::new();
    }
    let dash_boundary = [b"--".as_slice(), boundary].concat();
    let found = delimiters(body, &dash_boundary);
    let starts = found.iter().filter_map(|(_, d)| match d {
        Delimiter::Open { next } => Some(*next),
        Delimiter::Close => None,
    });
    let ends = found.iter().skip(1).map(|(at, _)| at - 2).chain(std::iter::once(body.len()));
    starts.zip(ends).map(|(start, end)| start..end).collect()
}

/// Whether `body` frames cleanly under `boundary`: its first delimiter opens a
/// part, and every line up to the close delimiter that starts with
/// `--boundary` is a delimiter. RFC 2046 §5.1.1 recognises a delimiter by the
/// start of its line, so a line that starts with one and is none (`--bb` under
/// `b`, a delimiter ended by LF alone, two delimiters sharing one CRLF) leaves
/// the body with no one reading, and so does a body that closes first.
pub(crate) fn frames_cleanly(body: &[u8], boundary: &str) -> bool {
    frames_cleanly_counting(body, boundary, &mut 0)
}

/// [`frames_cleanly`], one walk over the line starts and the delimiters
/// together, both ascending: each line start and each delimiter passed adds
/// one to `steps`.
fn frames_cleanly_counting(body: &[u8], boundary: &str, steps: &mut usize) -> bool {
    let dash_boundary = [b"--".as_slice(), boundary.as_bytes()].concat();
    let found = delimiters(body, &dash_boundary);
    if found.first().is_some_and(|(_, d)| *d == Delimiter::Close) {
        return false;
    }
    let close =
        found.last().filter(|(_, d)| *d == Delimiter::Close).map_or(body.len(), |(at, _)| *at);
    let line_starts = std::iter::once(0)
        .chain(memchr::memmem::find_iter(body, b"\r\n").map(|at| at + 2))
        .take_while(|&at| at <= close);
    let mut known = found.iter().map(|(at, _)| *at).peekable();
    for at in line_starts {
        *steps += 1;
        if !body.get(at..).is_some_and(|rest| rest.starts_with(&dash_boundary)) {
            continue;
        }
        while known.next_if(|&line| line < at).is_some() {
            *steps += 1;
        }
        if known.peek() != Some(&at) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_texts(body: &str) -> Vec<&str> {
        blocks(body.as_bytes(), b"b").into_iter().map(|r| &body[r]).collect()
    }

    #[test]
    fn a_delimiter_is_a_whole_line() {
        assert_eq!(block_texts("--b\r\none\r\n--b\r\ntwo\r\n--b--\r\n"), ["one", "two"]);
        assert_eq!(block_texts("pre\r\n--b\r\none\r\n--b--"), ["one"], "a preamble");
        assert_eq!(block_texts("\r\n--b\r\none\r\n--b--"), ["one"], "a blank preamble");
        assert_eq!(block_texts("--b \t\r\none\r\n--b--"), ["one"], "transport padding");
        assert_eq!(block_texts("--b\r\nx--b\r\n--b--"), ["x--b"], "mid-line text");
        assert_eq!(block_texts("--b\r\n--bb\r\n--b--"), ["--bb"], "a longer boundary");
        assert_eq!(block_texts("--b\r\n--b x\r\n--b--"), ["--b x"], "text after it");
        assert!(block_texts("--b\none\n--b--\n").is_empty(), "LF is no line end");
    }

    #[test]
    fn reading_stops_at_the_close_delimiter() {
        assert_eq!(block_texts("--b\r\none\r\n--b--\r\n--b\r\ntwo"), ["one"]);
        assert_eq!(block_texts("--b\r\none\r\n--b-- trailing"), ["one"]);
    }

    #[test]
    fn a_body_that_never_closes_keeps_its_last_part() {
        assert_eq!(block_texts("--b\r\none\r\n--b\r\ntwo\r\n"), ["one", "two\r\n"]);
        assert_eq!(block_texts("--b\r\none\r\n--b"), ["one", ""]);
    }

    #[test]
    fn a_line_starting_with_the_delimiter_is_one() {
        assert!(frames_cleanly(b"pre --b\r\n--b\r\none\r\n--b--\r\nepi\r\n--bb", "b"));
        assert!(frames_cleanly(b"--b \r\none\r\n--b--", "b"), "transport padding");
        assert!(!frames_cleanly(b"--b\r\none\r\n--bb\r\n--b--", "b"), "a longer line");
        assert!(!frames_cleanly(b"--bb\r\n--b\r\none\r\n--b--", "b"), "in the preamble");
        assert!(!frames_cleanly(b"--b\none\n--b--\n", "b"), "LF alone");
        assert!(!frames_cleanly(b"--b\r\n--b--\r\n", "b"), "one CRLF for two lines");
        assert!(!frames_cleanly(b"--b--\r\n--b\r\none\r\n--b--", "b"), "it closes first");
    }

    /// The walk is linear in the lines: a datagram's worth of delimiter lines,
    /// and twice that, cost one step per line start and per delimiter.
    #[test]
    fn framing_a_body_of_delimiter_lines_is_linear() {
        let steps = |lines: usize| {
            let body = "--b\r\n\r\n".repeat(lines);
            let mut steps = 0;
            assert!(frames_cleanly_counting(body.as_bytes(), "b", &mut steps));
            steps
        };
        let most = 65_535 / "--b\r\n\r\n".len();
        let (one, two) = (steps(most), steps(2 * most));
        assert!(one <= 3 * most + 1, "{one} steps for {most} lines");
        assert!(two <= 2 * one + 1, "{two} steps for twice {one}");
    }

    #[test]
    fn an_empty_part_and_an_empty_boundary() {
        assert_eq!(block_texts("--b\r\n\r\n--b--"), [""]);
        assert!(blocks(b"--\r\nx\r\n----", b"").is_empty());
    }
}
