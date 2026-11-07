//! The top-level entry splitter every list header reads through, and the
//! lenient CSeq reading of a message held to no wire grammar.
//!
//! We byte-scan the source `&str` directly: every structural delimiter in these
//! grammars is ASCII, so a UTF-8 lead or continuation byte can never alias one,
//! and every index we slice at lands on a char boundary.

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
pub fn top_level_entries(value: &str, sep: ItemSeparator) -> TopLevelEntries<'_> {
    TopLevelEntries { value, seps: sep.bytes(), pos: 0, emitted: 0, done: false }
}

/// The comma-separated entries of one header value — the SIP §7.3.1 default.
pub fn top_level_comma_entries(value: &str) -> TopLevelEntries<'_> {
    top_level_entries(value, ItemSeparator::Comma)
}

pub struct TopLevelEntries<'a> {
    value: &'a str,
    seps: &'static [u8],
    pos: usize,
    emitted: usize,
    done: bool,
}

impl<'a> Iterator for TopLevelEntries<'a> {
    type Item = &'a str;

    // Byte-scan rather than collecting a `Vec<char>` (4x the bytes, and the
    // single hottest parse frame under load). Every structural delimiter here
    // (`" \ < >` and the [`ItemSeparator`]) is ASCII, so a UTF-8
    // lead/continuation byte can never alias one, and each split index lands on
    // the separator — always a char boundary. Entries are trimmed *borrowed*
    // subslices; a caller that stores one calls `.to_string()` at the point of
    // ownership. Scanning resumes with depth 0 and no open quote because a
    // separator is only recognised in exactly that state.
    fn next(&mut self) -> Option<&'a str> {
        if self.done {
            return None;
        }
        let bytes = self.value.as_bytes();
        let start = self.pos;
        let mut depth: i32 = 0;
        let mut in_quote = false;
        let mut i = start;
        while let Some(&c) = bytes.get(i) {
            if in_quote {
                if c == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if c == b'"' {
                    in_quote = false;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' => in_quote = true,
                b'<' => depth += 1,
                b'>' if depth > 0 => depth -= 1,
                c if depth == 0 && self.seps.contains(&c) => {
                    self.pos = i + 1;
                    self.emitted += 1;
                    return Some(crate::whitespace::trim(self.value.get(start..i)?));
                }
                _ => {}
            }
            i += 1;
        }
        self.done = true;
        let tail = crate::whitespace::trim(self.value.get(start..)?);
        // A lone empty value has no entries; a trailing empty entry after a
        // separator is one.
        if tail.is_empty() && self.emitted == 0 {
            return None;
        }
        self.emitted += 1;
        Some(tail)
    }
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
    let mut i = 0;
    while let Some(b) = bytes.get(i).copied() {
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let hex = |k: usize| bytes.get(k).copied().and_then(|h| char::from(h).to_digit(16));
            let (hi, lo) = (hex(i + 1).ok_or(())?, hex(i + 2).ok_or(())?);
            out.push(u8::try_from(hi * 16 + lo).map_err(|_| ())?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}
