//! Structured SIP header extraction — quote-aware, zero-regex. Port of
//! `src/sip/parsers/custom/structured-headers.ts`.
//!
//! Scans From/To, Via, Contact and CSeq values into their parts, leniently:
//! the reading of a message held to no wire grammar, and the Via and CSeq
//! readings every message takes. The strict address readings are
//! [`NameAddr::parse`](crate::header::NameAddr::parse) and
//! [`Uri::parse`](crate::header::Uri::parse).
//!
//! We byte-scan the source `&str` directly: every structural delimiter in these
//! grammars is ASCII, so a UTF-8 lead or continuation byte can never alias one,
//! and every index we slice at lands on a char boundary. No function collects a
//! `Vec<char>` (4x the bytes plus an allocation per call).

use crate::header::{read_sent_by, ItemSeparator, ParamValue, Params};
use crate::sip_str::SipStr;

// ---------------------------------------------------------------------------
// Parsed types (parser-internal; mapped to public field types in extract_fields)
// ---------------------------------------------------------------------------
//
// Every field is a [`SipStr`] cut from the header value handed in, so parsing a
// structured header allocates nothing beyond the param map itself.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNameAddr {
    pub display_name: Option<SipStr>,
    pub uri: SipStr,
    pub tag: Option<SipStr>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVia {
    pub protocol: SipStr,
    pub version: SipStr,
    pub transport: SipStr,
    /// The sent-by host and port digits as the URI host reader reads them,
    /// or why it does not read.
    pub sent_by: Result<(SipStr, Option<u64>), String>,
    /// The sent-by as written, up to the first `;`: what a lenient reading
    /// holds whole when the reader refuses it.
    pub sent_by_text: SipStr,
    pub branch: Option<SipStr>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedContact {
    pub display_name: Option<SipStr>,
    pub uri: SipStr,
    pub params: Params,
}

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
    TopLevelEntries { value, sep: sep.byte(), pos: 0, emitted: 0, done: false }
}

/// The comma-separated entries of one header value — the SIP §7.3.1 default.
pub fn top_level_comma_entries(value: &str) -> TopLevelEntries<'_> {
    top_level_entries(value, ItemSeparator::Comma)
}

/// The `;`-separated entries of one header value — the layout RFC 3323 §4.2
/// gives the priv-value list.
pub fn top_level_semicolon_entries(value: &str) -> TopLevelEntries<'_> {
    top_level_entries(value, ItemSeparator::Semicolon)
}

pub struct TopLevelEntries<'a> {
    value: &'a str,
    sep: u8,
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
                c if c == self.sep && depth == 0 => {
                    self.pos = i + 1;
                    self.emitted += 1;
                    return Some(self.value.get(start..i)?.trim());
                }
                _ => {}
            }
            i += 1;
        }
        self.done = true;
        let tail = self.value.get(start..)?.trim();
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
// From / To parsing (name-addr with tag)
// ---------------------------------------------------------------------------

pub fn parse_name_addr(value: &SipStr) -> ParsedNameAddr {
    let s = value.as_bytes();
    let len = s.len();
    let mut i = skip_ws(s, 0);

    let mut display_name: Option<SipStr> = None;
    let uri: SipStr;

    if byte_at(s, i) == Some(b'"') {
        // Quoted display name.
        let (text, end) = read_quoted_string(value, i);
        display_name = Some(text);
        i = skip_ws(s, end);
        if byte_at(s, i) == Some(b'<') {
            match index_of(s, b'>', i + 1) {
                None => {
                    let uri = slice_trimmed(value, i + 1, len);
                    return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
                }
                Some(close) => {
                    uri = slice(value, i + 1, close);
                    i = close + 1;
                }
            }
        } else {
            let uri = slice_trimmed(value, i, len);
            return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
        }
    } else if let Some(open) = index_of(s, b'<', i) {
        let before = slice_trimmed(value, i, open);
        display_name = if before.is_empty() { None } else { Some(before) };
        match index_of(s, b'>', open + 1) {
            None => {
                let uri = slice_trimmed(value, open + 1, len);
                return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
            }
            Some(close) => {
                uri = slice(value, open + 1, close);
                i = close + 1;
            }
        }
    } else {
        // addr-spec (bare URI).
        match index_of(s, b';', i) {
            None => {
                let uri = slice_trimmed(value, i, len);
                return ParsedNameAddr {
                    display_name: None,
                    uri,
                    tag: None,
                    params: Params::new(),
                };
            }
            Some(semi) => {
                uri = slice_trimmed(value, i, semi);
                i = semi;
            }
        }
    }

    let params = parse_header_params(value, i);
    let tag = param_text(&params, "tag");

    ParsedNameAddr { display_name, uri, tag, params }
}

// ---------------------------------------------------------------------------
// Via parsing
// ---------------------------------------------------------------------------

pub fn parse_via(value: &SipStr) -> ParsedVia {
    let s = value.as_bytes();
    let mut i = skip_ws(s, 0);

    // sent-protocol: "SIP/2.0/UDP" or "SIP / 2.0 / TCP".
    let proto_end = scan_until_one_of(s, i, b"/");
    let protocol = slice_trimmed(value, i, proto_end);
    i = proto_end + 1;

    let ver_end = scan_until_one_of(s, i, b"/");
    let version = slice_trimmed(value, i, ver_end);
    i = ver_end + 1;

    i = skip_ws(s, i);
    let trans_end = scan_until_ws_or_semi(s, i);
    let transport = slice_trimmed(value, i, trans_end);
    i = trans_end;

    i = skip_ws(s, i);
    let text_end = scan_until_one_of(s, i, b";");
    let sent_by_text = slice_trimmed(value, i, text_end);
    let (sent_by, params_at) = match read_sent_by(value, i) {
        Ok((host, port, end)) => (Ok((host, port)), end),
        Err(reason) => (Err(reason), text_end),
    };

    let params = parse_header_params(value, params_at);
    let branch = param_text(&params, "branch");

    ParsedVia { protocol, version, transport, sent_by, sent_by_text, branch, params }
}

// ---------------------------------------------------------------------------
// Contact parsing
// ---------------------------------------------------------------------------

pub fn parse_contact(value: &SipStr) -> ParsedContact {
    let parsed = parse_name_addr(value);
    ParsedContact { display_name: parsed.display_name, uri: parsed.uri, params: parsed.params }
}

// ---------------------------------------------------------------------------
// CSeq parsing: "number method"
// ---------------------------------------------------------------------------

pub fn parse_cseq(value: &SipStr) -> ParsedCSeq {
    let s = value.as_bytes();
    let mut i = skip_ws(s, 0);
    let num_start = i;
    i = skip_digits(s, i);
    let seq = fold_digits(s, num_start, i);
    i = skip_ws(s, i);
    let method = slice_trimmed(value, i, s.len());
    ParsedCSeq { seq, method }
}

// ---------------------------------------------------------------------------
// Internal helpers — byte-index scanning over the source &str
// ---------------------------------------------------------------------------
// Every delimiter these helpers scan for is ASCII: a UTF-8 lead/continuation
// byte can never equal one, so byte-wise scanning visits exactly the same
// structural positions a `&[char]` walk would, and every index handed to
// a `&str` slice below lands on a char boundary. Indices may run past the end
// (the callers propagate `end + 1` positions on missing delimiters, as the TS
// did) — hence the clamping in `subslice`.

/// Borrowed `s[a..b]` with both byte indices clamped to `s.len()`; empty when
/// the clamped range is not one of `s` (`a` past `b`, or inside a character),
/// as it is when both indices run past the end.
fn subslice(s: &str, a: usize, b: usize) -> &str {
    s.get(a.min(s.len())..b.min(s.len())).unwrap_or("")
}

/// The byte at `i`, `None` past the end.
fn byte_at(s: &[u8], i: usize) -> Option<u8> {
    s.get(i).copied()
}

/// The index of the first non-digit byte at or after `i`.
fn skip_digits(s: &[u8], mut i: usize) -> usize {
    while byte_at(s, i).is_some_and(|b| b.is_ascii_digit()) {
        i += 1;
    }
    i
}

/// `base[a..b]` as a span of `base`, clamped like [`subslice`] — the single
/// point every structured field is read out at, and the reason a parsed
/// From/Via/Contact copies no bytes.
fn slice(base: &SipStr, a: usize, b: usize) -> SipStr {
    span_of(base, subslice(base.as_str(), a, b))
}

/// [`slice`] with surrounding whitespace excluded.
fn slice_trimmed(base: &SipStr, a: usize, b: usize) -> SipStr {
    span_of(base, subslice(base.as_str(), a, b).trim())
}

/// The text a parameter carries, as a span of the value it was read from.
fn param_text(params: &Params, name: &str) -> Option<SipStr> {
    match params.get(name)? {
        ParamValue::Flag => None,
        ParamValue::Token(v) | ParamValue::Quoted(v) => Some(v.clone()),
    }
}

/// Re-express `sub` — a slice of `base`'s text — as a span sharing `base`'s
/// buffer.
fn span_of(base: &SipStr, sub: &str) -> SipStr {
    base.reslice(sub)
}

fn index_of(s: &[u8], needle: u8, from: usize) -> Option<usize> {
    s.get(from..)?.iter().position(|&b| b == needle).map(|p| from + p)
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while matches!(byte_at(s, i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i
}

/// Read a quoted string whose opening `"` is at byte `i`. Returns the
/// unescaped text and the byte position after the closing `"`. An unescaped
/// run — the common case — comes back as a span; only a `\`-escape forces the
/// rebuilt owned copy.
fn read_quoted_string(base: &SipStr, mut i: usize) -> (SipStr, usize) {
    let s = base.as_str();
    let bytes = s.as_bytes();
    i += 1; // skip opening "
    let mut result: Option<String> = None;
    let mut run_start = i;
    while let Some(c) = byte_at(bytes, i) {
        // The escaped char may be multi-byte — copy it whole.
        let escaped =
            if c == b'\\' { s.get(i + 1..).and_then(|rest| rest.chars().next()) } else { None };
        if let Some(esc) = escaped {
            let out = result.get_or_insert_with(String::new);
            out.push_str(subslice(s, run_start, i));
            out.push(esc);
            i += 1 + esc.len_utf8();
            run_start = i;
            continue;
        }
        if c == b'"' {
            return (finish_quoted(base, result, s, run_start, i), i + 1);
        }
        i += 1;
    }
    (finish_quoted(base, result, s, run_start, bytes.len()), i)
}

/// Close out [`read_quoted_string`]: append the final unescaped run to the
/// rebuilt copy, or hand back the whole run as a span when there was none.
fn finish_quoted(
    base: &SipStr,
    rebuilt: Option<String>,
    s: &str,
    run_start: usize,
    end: usize,
) -> SipStr {
    match rebuilt {
        Some(mut out) => {
            out.push_str(subslice(s, run_start, end));
            SipStr::owned(&out)
        }
        None => span_of(base, subslice(s, run_start, end)),
    }
}

/// Scan forward until one of the (ASCII) delimiter bytes; returns its index
/// (or end, or `i` when it is past the end).
fn scan_until_one_of(s: &[u8], i: usize, delims: &[u8]) -> usize {
    scan_until(s, i, |c| delims.contains(&c))
}

/// The index of the first byte at or after `i` that `stop` holds for, the end
/// when none does, `i` itself when it is past the end.
fn scan_until(s: &[u8], i: usize, stop: impl Fn(u8) -> bool) -> usize {
    match s.get(i..) {
        Some(rest) => rest.iter().position(|&c| stop(c)).map_or(s.len(), |p| i + p),
        None => i,
    }
}

/// Scan until whitespace, `;`, or `,`.
fn scan_until_ws_or_semi(s: &[u8], i: usize) -> usize {
    scan_until(s, i, |c| matches!(c, b' ' | b'\t' | b';' | b','))
}

fn fold_digits(s: &[u8], from: usize, to: usize) -> u64 {
    s.iter()
        .take(to)
        .skip(from)
        .filter_map(|&c| char::from(c).to_digit(10))
        .fold(0u64, |n, d| n.saturating_mul(10).saturating_add(u64::from(d)))
}

/// Parse header-level parameters (after `>` or after addr-spec). This is where
/// `tag=` lives — semicolon-separated `key[=value]` at the HEADER level.
fn parse_header_params(base: &SipStr, i: usize) -> Params {
    let mut params: Params = Params::new();
    collect_params_from(base, i, &mut params);
    params
}

/// Read `;`-separated params from byte `i` to the end of `base`.
fn collect_params_from(base: &SipStr, mut i: usize, params: &mut Params) {
    let bytes = base.as_bytes();
    let len = bytes.len();
    while i < len {
        i = skip_ws(bytes, i);
        let Some(c) = byte_at(bytes, i) else {
            break;
        };
        if c != b';' {
            // Skip unexpected bytes (commas for multi-value headers, etc.).
            i += 1;
            continue;
        }
        i += 1; // skip ;
        i = read_one_param(base, skip_ws(bytes, i), params);
    }
}

/// Read one `key[=value]` starting at byte `i`; returns the position after it.
fn read_one_param(base: &SipStr, mut i: usize, params: &mut Params) -> usize {
    let s = base.as_str();
    let bytes = s.as_bytes();

    let name_end = scan_until_one_of(bytes, i, b"=; \t,>");
    let pname = slice(base, i, name_end);
    i = name_end;

    // RFC 3261 EQUAL permits surrounding LWS: `SWS "=" SWS`.
    i = skip_ws(bytes, i);
    if byte_at(bytes, i) == Some(b'=') {
        i += 1;
        i = skip_ws(bytes, i);
        if byte_at(bytes, i) == Some(b'"') {
            let (text, end) = read_quoted_string(base, i);
            params.push(pname, ParamValue::Quoted(text));
            i = end;
        } else {
            let val_end = scan_until_one_of(bytes, i, b";, \t>");
            params.push(pname, ParamValue::Token(slice(base, i, val_end)));
            i = val_end;
        }
    } else if !pname.is_empty() {
        params.push(pname, ParamValue::Flag);
    }
    i
}

/// Mimic JS `decodeURIComponent`: decode `%XX` as UTF-8 bytes; `Err` on a
/// malformed escape or invalid UTF-8 (the caller falls back to the raw form).
pub(crate) fn decode_uri_component(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(b) = byte_at(bytes, i) {
        if b == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let hex = |k: usize| byte_at(bytes, k).and_then(|h| char::from(h).to_digit(16));
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
