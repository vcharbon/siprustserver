//! Byte-level scanning primitives shared by the header value parsers.
//!
//! Every structural delimiter in the SIP header grammars is ASCII, so a UTF-8
//! lead or continuation byte can never alias one: byte-wise scanning visits
//! exactly the structural positions a char walk would, and every index handed
//! to a slice lands on a char boundary. Indices may run past the end — the
//! slicing helpers clamp.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

/// The byte at `i`, `None` past the end.
pub(crate) fn byte_at(s: &[u8], i: usize) -> Option<u8> {
    s.get(i).copied()
}

pub(crate) fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while matches!(byte_at(s, i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i
}

pub(crate) fn index_of(s: &[u8], needle: u8, from: usize) -> Option<usize> {
    s.get(from..)?.iter().position(|&b| b == needle).map(|p| from + p)
}

/// Scan forward until one of the (ASCII) delimiter bytes, or the end; `i`
/// itself when it is past the end.
pub(crate) fn scan_until(s: &[u8], i: usize, delims: &[u8]) -> usize {
    match s.get(i..) {
        Some(rest) => rest.iter().position(|c| delims.contains(c)).map_or(s.len(), |p| i + p),
        None => i,
    }
}

/// `text[a..b]`, both indices clamped; empty when the clamped range is not one
/// of `text` (`a` past `b`, or inside a character).
pub(crate) fn clamped(text: &str, a: usize, b: usize) -> &str {
    text.get(a.min(text.len())..b.min(text.len())).unwrap_or("")
}

/// `base[a..b]` as a span of `base`, both indices clamped — the single
/// materialization point for every field, and the reason a parsed value copies
/// no bytes.
pub(crate) fn sub(base: &SipStr, a: usize, b: usize) -> SipStr {
    base.reslice(clamped(base.as_str(), a, b))
}

/// [`sub`] with surrounding whitespace excluded.
pub(crate) fn sub_trimmed(base: &SipStr, a: usize, b: usize) -> SipStr {
    base.reslice(clamped(base.as_str(), a, b).trim())
}

/// [`sub`] lowercased — a span when it is already lowercase, an owned copy only
/// when a fold is needed. For the case-insensitive tokens (scheme, transport)
/// whose canonical form this crate emits.
pub(crate) fn sub_lower(base: &SipStr, a: usize, b: usize) -> SipStr {
    let s = sub(base, a, b);
    if s.chars().any(char::is_uppercase) {
        SipStr::owned(&s.to_lowercase())
    } else {
        s
    }
}

/// Read a quoted string whose opening `"` sits at byte `i`. Yields the
/// UNESCAPED text and the position after the closing `"`. An escape-free run —
/// the common case — comes back as a span; only a `\` forces an owned copy.
pub(crate) fn read_quoted(base: &SipStr, mut i: usize) -> (SipStr, usize) {
    let s = base.as_str();
    let bytes = s.as_bytes();
    i += 1;
    let mut rebuilt: Option<String> = None;
    let mut run_start = i;
    while let Some(c) = byte_at(bytes, i) {
        let escaped =
            if c == b'\\' { s.get(i + 1..).and_then(|rest| rest.chars().next()) } else { None };
        if let Some(esc) = escaped {
            let out = rebuilt.get_or_insert_with(String::new);
            out.push_str(clamped(s, run_start, i));
            out.push(esc);
            i += 1 + esc.len_utf8();
            run_start = i;
            continue;
        }
        if c == b'"' {
            return (finish_quoted(base, rebuilt, s, run_start, i), i + 1);
        }
        i += 1;
    }
    (finish_quoted(base, rebuilt, s, run_start, bytes.len()), i)
}

fn finish_quoted(
    base: &SipStr,
    rebuilt: Option<String>,
    s: &str,
    run_start: usize,
    end: usize,
) -> SipStr {
    match rebuilt {
        Some(mut out) => {
            out.push_str(clamped(s, run_start, end));
            SipStr::owned(&out)
        }
        None => base.reslice(clamped(s, run_start, end)),
    }
}

/// The end of the ASCII digit run starting at `i`.
pub(crate) fn digits_end(s: &[u8], mut i: usize) -> usize {
    while byte_at(s, i).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
    }
    i
}

/// A `1*DIGIT` field within the `u32` range. Empty, non-numeric or overflowing
/// input is a parse error rather than a saturated value — a stack that rounds a
/// sequence number silently corrupts a dialog.
pub(crate) fn parse_u32(text: &str, field: &str) -> Result<u32, SipParseError> {
    let t = text.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SipParseError::new(format!("{field} is not 1*DIGIT: {t:?}")));
    }
    t.parse::<u32>().map_err(|_| SipParseError::new(format!("{field} out of range: {t:?}")))
}

/// A port: `1*DIGIT` within the 16-bit range. `None` when the field is absent.
pub(crate) fn parse_port(text: &str) -> Result<u16, SipParseError> {
    let t = text.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SipParseError::new(format!("non-digit in port: {t:?}")));
    }
    t.parse::<u16>().map_err(|_| SipParseError::new(format!("port out of range: {t:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_shares_the_source_buffer() {
        let base = SipStr::owned("sip:alice@host");
        let host = sub(&base, 10, 14);
        assert_eq!(host.as_str(), "host");
        assert_eq!(host.as_str().as_ptr(), base.as_str()[10..].as_ptr());
    }

    #[test]
    fn an_escape_free_quoted_run_is_a_span() {
        let base = SipStr::owned("\"Alice\" <sip:a@h>");
        let (text, end) = read_quoted(&base, 0);
        assert_eq!(text.as_str(), "Alice");
        assert_eq!(end, 7);
        assert_eq!(text.as_str().as_ptr(), base.as_str()[1..].as_ptr());
    }

    #[test]
    fn an_escaped_quoted_run_is_unescaped_into_a_copy() {
        let base = SipStr::owned(r#""a\"b""#);
        let (text, _) = read_quoted(&base, 0);
        assert_eq!(text.as_str(), "a\"b");
    }

    #[test]
    fn out_of_range_numbers_are_errors_not_saturations() {
        assert!(parse_u32("4294967296", "CSeq").is_err());
        assert!(parse_port("88161").is_err());
        assert_eq!(parse_port("5060").unwrap(), 5060);
    }
}
