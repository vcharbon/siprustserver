//! Structured SIP header extraction — quote-aware, zero-regex. Port of
//! `src/sip/parsers/custom/structured-headers.ts`.
//!
//! Parses From/To, Via, Contact, CSeq and SIP URIs, plus the strict host /
//! SIP-URI validators (ADR-0007). These are the exact functions the ABNF fuzz
//! suite drives.
//!
//! The TS source scans JS strings by UTF-16 code unit with `charCodeAt` /
//! `slice` / `indexOf`. We byte-scan the source `&str` directly: every
//! structural delimiter in these grammars is ASCII, so a UTF-8 lead or
//! continuation byte can never alias one, and every index we slice at lands on
//! a char boundary. (Previously each function collected a `Vec<char>` per call
//! — 4x the bytes plus an allocation, the top self-time bucket under load.)

use std::collections::BTreeMap;

use crate::header::{ParamValue, Params};
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
    pub host: SipStr,
    pub port: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUri {
    pub scheme: SipStr,
    pub user: Option<SipStr>,
    pub host: SipStr,
    pub port: Option<u64>,
    pub params: BTreeMap<SipStr, SipStr>,
}

// ---------------------------------------------------------------------------
// Top-level entry splitter — quote-aware and angle-bracket-aware.
// ---------------------------------------------------------------------------

/// The `sep`-separated entries of one header value, trimmed and borrowed. A
/// separator inside a quoted-string or `<...>` is data. An empty value yields
/// nothing; an empty entry between two separators is yielded. `sep` must be
/// ASCII — the byte scan below relies on it.
pub fn top_level_entries(value: &str, sep: u8) -> TopLevelEntries<'_> {
    TopLevelEntries { value, sep, pos: 0, emitted: 0, done: false }
}

/// The comma-separated entries of one header value — the SIP §7.3.1 default.
pub fn top_level_comma_entries(value: &str) -> TopLevelEntries<'_> {
    top_level_entries(value, b',')
}

/// The `;`-separated entries of one header value — the layout RFC 3323 §4.2
/// gives the priv-value list.
pub fn top_level_semicolon_entries(value: &str) -> TopLevelEntries<'_> {
    top_level_entries(value, b';')
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
    // (`" \ < >` and the separator) is ASCII, so a UTF-8 lead/continuation byte
    // can never alias one, and each split index lands on the separator — always
    // a char boundary — so slicing by byte index is panic-free. Entries are trimmed *borrowed*
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
        while i < bytes.len() {
            let c = bytes[i];
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
                    return Some(self.value[start..i].trim());
                }
                _ => {}
            }
            i += 1;
        }
        self.done = true;
        let tail = self.value[start..].trim();
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

    if i < len && s[i] == b'"' {
        // Quoted display name.
        let (text, end) = read_quoted_string(value, i);
        display_name = Some(text);
        i = skip_ws(s, end);
        if i < len && s[i] == b'<' {
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
    let (host, port, host_end) = parse_host_port(value, i);
    i = host_end;

    let params = parse_header_params(value, i);
    let branch = param_text(&params, "branch");

    ParsedVia { protocol, version, transport, host, port, branch, params }
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
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let seq = fold_digits(s, num_start, i);
    i = skip_ws(s, i);
    let method = slice_trimmed(value, i, s.len());
    ParsedCSeq { seq, method }
}

// ---------------------------------------------------------------------------
// SIP URI parsing
// ---------------------------------------------------------------------------

/// Locate the `?` opening embedded URI-headers (RFC 3261 §19.1.1) — the one
/// after hostport, anchored past userinfo `@` so a userinfo `?` is not
/// misidentified. `None` when there are no embedded headers. The returned
/// index is a BYTE offset into `uri` (always on a char boundary — `?` is
/// ASCII), directly usable with `&uri[..q]` / `&uri[q + 1..]`.
pub fn find_uri_embedded_headers_start(uri: &str) -> Option<usize> {
    let s = uri.as_bytes();
    let colon_idx = index_of(s, b':', 0)?;
    let host_start = match index_of(s, b'@', colon_idx + 1) {
        None => colon_idx + 1,
        Some(a) => a + 1,
    };
    index_of(s, b'?', host_start)
}

pub fn parse_sip_uri_string(uri: &SipStr) -> Option<ParsedUri> {
    let s = uri.as_bytes();
    let len = s.len();

    let colon_idx = index_of(s, b':', 0)?;
    let scheme = slice_lowercased(uri, 0, colon_idx);
    let mut i = colon_idx + 1;

    let user: Option<SipStr>;
    let host_start: usize;

    let at_idx = scan_until_one_of(s, i, b"@>");
    if at_idx < len && s[at_idx] == b'@' {
        user = Some(slice(uri, i, at_idx));
        host_start = at_idx + 1;
    } else {
        user = None;
        host_start = i;
    }

    let (host, port, host_end) = parse_host_port(uri, host_start);
    i = host_end;

    let mut params: BTreeMap<SipStr, SipStr> = BTreeMap::new();
    while i < len && s[i] == b';' {
        i += 1;
        let name_end = scan_until_one_of(s, i, b"=;>? \t");
        let pname = slice_lowercased(uri, i, name_end);
        i = name_end;
        if i < len && s[i] == b'=' {
            i += 1;
            let val_end = scan_until_one_of(s, i, b";>? \t");
            params.insert(pname, slice(uri, i, val_end));
            i = val_end;
        } else {
            params.insert(pname, SipStr::EMPTY);
        }
    }

    Some(ParsedUri { scheme, user, host, port, params })
}

// ---------------------------------------------------------------------------
// Internal helpers — byte-index scanning over the source &str
// ---------------------------------------------------------------------------
// Every delimiter these helpers scan for is ASCII: a UTF-8 lead/continuation
// byte can never equal one, so byte-wise scanning visits exactly the same
// structural positions the old `&[char]` walk did, and every index handed to
// a `&str` slice below lands on a char boundary. Indices may run past the end
// (the callers propagate `end + 1` positions on missing delimiters, as the TS
// did) — hence the clamping in `subslice`.

/// Borrowed `s[a..b]` with both byte indices clamped to `s.len()`.
fn subslice(s: &str, a: usize, b: usize) -> &str {
    &s[a.min(s.len())..b.min(s.len())]
}

/// `base[a..b]` as a span of `base`, clamped like [`subslice`] — the single
/// materialization point for every structured field, and the reason a parsed
/// From/Via/Contact copies no bytes.
fn slice(base: &SipStr, a: usize, b: usize) -> SipStr {
    span_of(base, subslice(base.as_str(), a, b))
}

/// [`slice`] with surrounding whitespace excluded.
fn slice_trimmed(base: &SipStr, a: usize, b: usize) -> SipStr {
    span_of(base, subslice(base.as_str(), a, b).trim())
}

/// `base[a..b]` lowercased — a span when it is already lowercase (the common
/// case for wire param names), an owned copy only when a fold is needed.
fn slice_lowercased(base: &SipStr, a: usize, b: usize) -> SipStr {
    let s = subslice(base.as_str(), a, b);
    if s.chars().any(char::is_uppercase) {
        SipStr::owned(&s.to_lowercase())
    } else {
        span_of(base, s)
    }
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
    s[from.min(s.len())..].iter().position(|&b| b == needle).map(|p| from + p)
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
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
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let out = result.get_or_insert_with(String::new);
            out.push_str(&s[run_start..i]);
            // The escaped char may be multi-byte — copy it whole.
            let esc = s[i + 1..].chars().next().unwrap();
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
            out.push_str(&s[run_start..end]);
            SipStr::owned(&out)
        }
        None => span_of(base, &s[run_start..end]),
    }
}

/// Scan forward until one of the (ASCII) delimiter bytes; returns its index
/// (or end).
fn scan_until_one_of(s: &[u8], mut i: usize, delims: &[u8]) -> usize {
    while i < s.len() {
        if delims.contains(&s[i]) {
            return i;
        }
        i += 1;
    }
    i
}

/// Scan until whitespace, `;`, or `,`.
fn scan_until_ws_or_semi(s: &[u8], mut i: usize) -> usize {
    while i < s.len() {
        let c = s[i];
        if c == b' ' || c == b'\t' || c == b';' || c == b',' {
            return i;
        }
        i += 1;
    }
    i
}

fn fold_digits(s: &[u8], from: usize, to: usize) -> u64 {
    let mut n: u64 = 0;
    for &c in &s[from..to] {
        if c.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add((c - b'0') as u64);
        }
    }
    n
}

/// Parse host[:port] from byte `i`. Host can be IPv4, bracketed IPv6, or
/// hostname.
fn parse_host_port(base: &SipStr, i: usize) -> (SipStr, Option<u64>, usize) {
    let s = base.as_str();
    let bytes = s.as_bytes();
    let len = bytes.len();

    // IPv6: [address]
    if i < len && bytes[i] == b'[' {
        match index_of(bytes, b']', i + 1) {
            None => return (slice(base, i + 1, len), None, len),
            Some(close) => {
                let host = slice(base, i + 1, close);
                let mut j = close + 1;
                let mut port: Option<u64> = None;
                if j < len && bytes[j] == b':' {
                    j += 1;
                    let port_start = j;
                    while j < len && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    // TS does not guard the IPv6 branch — empty digits yield NaN
                    // (an invalid port). We mirror with fold_digits → 0, which
                    // `is_valid_port` likewise rejects.
                    port = Some(fold_digits(bytes, port_start, j));
                }
                return (host, port, j);
            }
        }
    }

    // IPv4 or hostname: scan until : ; , > SP HTAB ?
    let host_end = scan_until_one_of(bytes, i, b":;,> \t?");
    let host = slice(base, i, host_end);

    let mut j = host_end;
    let mut port: Option<u64> = None;
    if j < len && bytes[j] == b':' {
        j += 1;
        let port_start = j;
        while j < len && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > port_start {
            port = Some(fold_digits(bytes, port_start, j));
        }
    }

    (host, port, j)
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
        if i >= len {
            break;
        }
        if bytes[i] != b';' {
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
    let len = bytes.len();

    let name_end = scan_until_one_of(bytes, i, b"=; \t,>");
    let pname = slice(base, i, name_end);
    i = name_end;

    // RFC 3261 EQUAL permits surrounding LWS: `SWS "=" SWS`.
    i = skip_ws(bytes, i);
    if i < len && bytes[i] == b'=' {
        i += 1;
        i = skip_ws(bytes, i);
        if i < len && bytes[i] == b'"' {
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

// ---------------------------------------------------------------------------
// Strict host validation (ADR-0007)
// ---------------------------------------------------------------------------

/// Validate a host already extracted by `parse_host_port`. `None` when
/// well-formed; `Some(reason)` otherwise. Empty host passes — callers enforce
/// non-empty where the grammar requires it.
pub fn validate_strict_host(host: &str) -> Option<String> {
    if host.is_empty() {
        return None;
    }
    // Any `:` means it was an IPv6 host (brackets already stripped); pass.
    if host.contains(':') {
        return None;
    }

    let labels: Vec<&str> = host.split('.').collect();
    let mut ipv4_shape = labels.len() == 4;
    if ipv4_shape {
        for label in &labels {
            if label.is_empty() || !label.bytes().all(|b| b.is_ascii_digit()) {
                ipv4_shape = false;
                break;
            }
        }
    }
    if ipv4_shape {
        for label in &labels {
            if label.len() > 1 && label.starts_with('0') {
                return Some(format!(
                    "IPv4 octet \"{label}\" has leading zero (octal-confusion vector)"
                ));
            }
            if label.len() > 3 {
                return Some(format!("IPv4 octet \"{label}\" exceeds 1*3DIGIT"));
            }
            let n: u64 = label.bytes().fold(0u64, |acc, b| acc * 10 + (b - b'0') as u64);
            if n > 255 {
                return Some(format!("IPv4 octet {n} out of range"));
            }
        }
        return None;
    }

    // Hostname: every label non-empty + alphanum-start, except a single
    // trailing empty label (the "host." absolute form).
    for (i, label) in labels.iter().enumerate() {
        if label.is_empty() {
            if i == labels.len() - 1 && labels.len() > 1 {
                continue; // trailing dot
            }
            return Some("empty host label".to_string());
        }
        let c0 = label.as_bytes()[0];
        let is_alpha = c0.is_ascii_alphabetic();
        let is_num = c0.is_ascii_digit();
        if !is_alpha && !is_num {
            return Some(format!("host label \"{label}\" must start with alphanum"));
        }
    }
    None
}

/// Count colons in a raw sent-by token; `Some(count)` when it exceeds 1 (the
/// `host [":" port]` grammar admits at most one). IPv6 refs bypass upstream.
pub fn detect_sent_by_multiple_colons(sent_by: &str) -> Option<usize> {
    let count = sent_by.bytes().filter(|&b| b == b':').count();
    if count > 1 {
        Some(count)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Strict SIP-URI grammar (ADR-0007)
// ---------------------------------------------------------------------------

/// Validate a SIP/SIPS URI string. `None` on success, `Some(reason)` on
/// failure. Non-SIP schemes pass with only structural checks.
pub fn validate_strict_sip_uri(uri: &str) -> Option<String> {
    let s = uri.as_bytes();
    if s.is_empty() {
        return Some("empty URI".to_string());
    }
    // Locate scheme colon.
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if c == b':' {
            break;
        }
        if i == 0 {
            if !c.is_ascii_alphabetic() {
                return Some("scheme must start with ALPHA".to_string());
            }
        } else {
            let is_alnum = c.is_ascii_alphanumeric();
            let is_special = c == b'+' || c == b'-' || c == b'.';
            if !is_alnum && !is_special {
                return Some("invalid scheme character".to_string());
            }
        }
        i += 1;
    }
    if i >= s.len() {
        return Some("missing scheme colon".to_string());
    }
    // Scheme bytes are validated ASCII above, so the case-fold compare is
    // exactly the old `to_lowercase()` — without minting a String per call.
    let scheme = &uri[..i];
    i += 1; // past ':'

    if !scheme.eq_ignore_ascii_case("sip") && !scheme.eq_ignore_ascii_case("sips") {
        return None;
    }

    // Locate the `@` terminating userinfo, if any.
    let mut at_idx: Option<usize> = None;
    let mut second_at = false;
    for j in i..s.len() {
        let c = s[j];
        if c == b'@' {
            if at_idx.is_none() {
                at_idx = Some(j);
            } else {
                second_at = true;
                break;
            }
        } else if c == b'>' {
            break;
        }
    }
    if second_at {
        return Some("multiple `@` in userinfo".to_string());
    }

    let host_start = match at_idx {
        Some(a) => {
            if a == i {
                return Some("empty user before `@`".to_string());
            }
            a + 1
        }
        None => i,
    };

    if host_start >= s.len() {
        return Some("empty hostport".to_string());
    }
    let first_host_byte = s[host_start];
    if first_host_byte == b':' {
        return Some("hostport starts with `:`".to_string());
    }
    if first_host_byte == b';' || first_host_byte == b'?' || first_host_byte == b'>' {
        return Some("empty hostport".to_string());
    }

    // End of hostport: first `;`, `?`, or `>`.
    let mut host_end = s.len();
    for j in host_start..s.len() {
        let c = s[j];
        if c == b';' || c == b'?' || c == b'>' {
            host_end = j;
            break;
        }
    }

    if s[host_start] == b'[' {
        // IPv6 reference: must close with `]`.
        let mut closed = false;
        let mut k = host_start + 1;
        while k < host_end {
            if s[k] == b']' {
                closed = true;
                break;
            }
            k += 1;
        }
        if !closed {
            return Some("unclosed IPv6 reference".to_string());
        }
        if k == host_start + 1 {
            return Some("empty IPv6 reference".to_string());
        }
        let mut after = k + 1;
        if after < host_end {
            if s[after] != b':' {
                return Some("junk after `]` in hostport".to_string());
            }
            after += 1;
            if let Some(reason) = validate_port_digits(s, after, host_end) {
                return Some(reason);
            }
        }
        return None;
    }

    // Plain host[:port]. Multiple unbracketed `:` = malformed.
    let mut colon_idx: Option<usize> = None;
    for j in host_start..host_end {
        if s[j] == b':' {
            if colon_idx.is_none() {
                colon_idx = Some(j);
            } else {
                return Some("multiple `:` in hostport".to_string());
            }
        }
    }
    let host = match colon_idx {
        None => subslice(uri, host_start, host_end),
        Some(c) => subslice(uri, host_start, c),
    };
    if host.is_empty() {
        return Some("empty host".to_string());
    }
    if let Some(reason) = validate_strict_host(host) {
        return Some(reason);
    }
    if let Some(c) = colon_idx {
        if let Some(reason) = validate_port_digits(s, c + 1, host_end) {
            return Some(reason);
        }
    }
    None
}

fn validate_port_digits(s: &[u8], from: usize, to: usize) -> Option<String> {
    if from >= to {
        return Some("empty port".to_string());
    }
    let mut n: u64 = 0;
    for &c in &s[from..to] {
        if !c.is_ascii_digit() {
            return Some("non-digit in port".to_string());
        }
        n = n * 10 + (c - b'0') as u64;
        if n > 65535 {
            return Some("port out of range".to_string());
        }
    }
    if n < 1 {
        return Some("port out of range".to_string());
    }
    None
}

/// Mimic JS `decodeURIComponent`: decode `%XX` as UTF-8 bytes; `Err` on a
/// malformed escape or invalid UTF-8 (the caller falls back to the raw form).
pub(crate) fn decode_uri_component(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let hi = (bytes[i + 1] as char).to_digit(16).ok_or(())?;
            let lo = (bytes[i + 2] as char).to_digit(16).ok_or(())?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}
