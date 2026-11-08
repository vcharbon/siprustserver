//! The top-level entry splitter every list header reads through, and the
//! lenient CSeq reading of a message held to no wire grammar.
//!
//! We byte-scan the source `&str` directly: every structural delimiter in these
//! grammars is ASCII, so a UTF-8 lead or continuation byte can never alias one,
//! and every index we slice at lands on a char boundary.

use std::ops::ControlFlow::{Break, Continue};

use crate::cursor::{items, walk};
use crate::header::ItemSeparator;
use crate::sip_str::SipStr;

/// The lenient reading of a CSeq: the leading digits saturated past `u64`,
/// and the rest as the method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCSeq {
    pub seq: u64,
    pub method: SipStr,
}

// ---------------------------------------------------------------------------
// Top-level entry splitter — quote-aware and angle-bracket-aware.
// ---------------------------------------------------------------------------

/// The `sep`-separated entries of one header value, trimmed and borrowed. A
/// separator inside a quoted-string or `<...>` is data. An empty value yields
/// nothing; an empty entry between two separators is yielded.
pub fn top_level_entries(value: &str, sep: ItemSeparator) -> impl Iterator<Item = &str> {
    let seps = sep.bytes();
    items(value.as_bytes(), move |from| entry_at(value, seps, from))
}

/// The comma-separated entries of one header value — the SIP §7.3.1 default.
pub fn top_level_comma_entries(value: &str) -> impl Iterator<Item = &str> {
    top_level_entries(value, ItemSeparator::Comma)
}

/// The entry of `value` starting at byte `start`, trimmed, and the position
/// after its separator; the last entry ends past the value's end.
///
/// A byte scan rather than a `Vec<char>` (4x the bytes, and the single hottest
/// parse frame under load): every structural delimiter here (`" \ < >` and the
/// [`ItemSeparator`]) is ASCII, so a UTF-8 lead/continuation byte can never
/// alias one, and each split index lands on a char boundary. A separator is
/// only recognised at depth 0 outside a quote, so each entry starts in that
/// state.
fn entry_at<'a>(value: &'a str, seps: &[u8], start: usize) -> Option<(&'a str, usize)> {
    let bytes = value.as_bytes();
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let separator = walk(bytes, start, |i| {
        let Some(&c) = bytes.get(i) else { return Break(None) };
        if in_quote {
            if c == b'\\' && i + 1 < bytes.len() {
                return Continue(i + 2);
            }
            if c == b'"' {
                in_quote = false;
            }
            return Continue(i + 1);
        }
        match c {
            b'"' => in_quote = true,
            b'<' => depth += 1,
            b'>' if depth > 0 => depth -= 1,
            c if depth == 0 && seps.contains(&c) => return Break(Some(i)),
            _ => {}
        }
        Continue(i + 1)
    });
    if let Ok(Some(at)) = separator {
        return Some((crate::whitespace::trim(value.get(start..at)?), at + 1));
    }
    let tail = crate::whitespace::trim(value.get(start..)?);
    // A lone empty value has no entries; a trailing empty entry after a
    // separator is one.
    if tail.is_empty() && start == 0 {
        return None;
    }
    Some((tail, bytes.len() + 1))
}

/// The entries of [`top_level_comma_entries`] collected — for callers that need
/// the list twice or by index.
pub fn split_top_level_commas(value: &str) -> Vec<&str> {
    top_level_comma_entries(value).collect()
}

// ---------------------------------------------------------------------------
// CSeq parsing: "number method"
// ---------------------------------------------------------------------------

pub fn parse_cseq(value: &SipStr) -> ParsedCSeq {
    let text = value.as_str().trim_start_matches([' ', '\t']);
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let seq = text
        .bytes()
        .take(digits)
        .fold(0u64, |n, d| n.saturating_mul(10).saturating_add(u64::from(d.saturating_sub(b'0'))));
    let method = text
        .get(digits..)
        .map_or(SipStr::EMPTY, |rest| value.reslice(crate::whitespace::trim(rest)));
    ParsedCSeq { seq, method }
}

/// Percent-decode `s` (each `%XX` one byte) as UTF-8; `Err` on a malformed
/// escape or bytes that are not UTF-8 (the caller falls back to the raw form).
pub(crate) fn decode_uri_component(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let hex = |k: usize| bytes.get(k).copied().and_then(|h| char::from(h).to_digit(16));
    let decoded = walk(bytes, 0, |i| match bytes.get(i).copied() {
        None => Break(Ok(())),
        Some(b'%') => {
            let byte =
                hex(i + 1).zip(hex(i + 2)).and_then(|(hi, lo)| u8::try_from(hi * 16 + lo).ok());
            match byte {
                Some(byte) => {
                    out.push(byte);
                    Continue(i + 3)
                }
                None => Break(Err(())),
            }
        }
        Some(b) => {
            out.push(b);
            Continue(i + 1)
        }
    });
    decoded.map_err(|_| ())??;
    String::from_utf8(out).map_err(|_| ())
}
