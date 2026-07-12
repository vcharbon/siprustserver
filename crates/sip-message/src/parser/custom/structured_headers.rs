//! Structured SIP header extraction — quote-aware, zero-regex. Port of
//! `src/sip/parsers/custom/structured-headers.ts`.
//!
//! Parses From/To, Via, Contact, CSeq, RAck, Replaces, Refer-To and SIP URIs,
//! plus the strict host / SIP-URI validators (ADR-0007). These are the exact
//! functions the ABNF fuzz suite drives.
//!
//! The TS source scans JS strings by UTF-16 code unit with `charCodeAt` /
//! `slice` / `indexOf`. We scan `&[char]` (Unicode scalars) so index math is
//! panic-free; for the ASCII/BMP inputs SIP carries this matches the source.

use std::collections::BTreeMap;

use crate::types::{ParamValue, Params};

// ---------------------------------------------------------------------------
// Parsed types (parser-internal; mapped to public field types in extract_fields)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNameAddr {
    pub display_name: Option<String>,
    pub uri: String,
    pub tag: Option<String>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVia {
    pub protocol: String,
    pub version: String,
    pub transport: String,
    pub host: String,
    pub port: Option<u64>,
    pub branch: Option<String>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedContact {
    pub display_name: Option<String>,
    pub uri: String,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCSeq {
    pub seq: u64,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRack {
    pub rseq: u64,
    pub seq: u64,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReplaces {
    pub call_id: String,
    pub to_tag: String,
    pub from_tag: String,
    pub early_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u64>,
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReferTo {
    pub display_name: Option<String>,
    pub uri: String,
    pub parsed_uri: Option<ParsedUri>,
    pub params: Params,
    pub embedded_headers: BTreeMap<String, String>,
    pub replaces: Option<ParsedReplaces>,
}

// ---------------------------------------------------------------------------
// Top-level comma splitter — quote-aware and angle-bracket-aware.
// ---------------------------------------------------------------------------

pub fn split_top_level_commas(value: &str) -> Vec<String> {
    // Byte-scan rather than collecting a `Vec<char>` (4x the bytes, and the
    // single hottest parse frame under load). Every structural delimiter here
    // (`" \ < > ,`) is ASCII, so a UTF-8 lead/continuation byte can never alias
    // one, and each split index lands on a `,` — always a char boundary — so
    // slicing the original `&str` by byte index is panic-free and byte-identical
    // to the old scalar walk.
    let bytes = value.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_quote = false;
    let mut start = 0usize;
    let mut i = 0usize;
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
            b'"' => {
                in_quote = true;
            }
            b'<' => {
                depth += 1;
            }
            b'>' if depth > 0 => {
                depth -= 1;
            }
            b',' if depth == 0 => {
                out.push(value[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = value[start..].trim().to_string();
    if !tail.is_empty() || !out.is_empty() {
        out.push(tail);
    }
    out
}

// ---------------------------------------------------------------------------
// From / To parsing (name-addr with tag)
// ---------------------------------------------------------------------------

pub fn parse_name_addr(value: &str) -> ParsedNameAddr {
    let s: Vec<char> = value.chars().collect();
    let len = s.len();
    let mut i = skip_ws(&s, 0);

    let mut display_name: Option<String> = None;
    let uri: String;

    if i < len && s[i] == '"' {
        // Quoted display name.
        let (text, end) = read_quoted_string(&s, i);
        display_name = Some(text);
        i = skip_ws(&s, end);
        if i < len && s[i] == '<' {
            match index_of(&s, '>', i + 1) {
                None => {
                    let uri = slice(&s, i + 1, len).trim().to_string();
                    return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
                }
                Some(close) => {
                    uri = slice(&s, i + 1, close);
                    i = close + 1;
                }
            }
        } else {
            let uri = slice(&s, i, len).trim().to_string();
            return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
        }
    } else if let Some(open) = index_of(&s, '<', i) {
        let before = slice(&s, i, open).trim().to_string();
        display_name = if before.is_empty() { None } else { Some(before) };
        match index_of(&s, '>', open + 1) {
            None => {
                let uri = slice(&s, open + 1, len).trim().to_string();
                return ParsedNameAddr { display_name, uri, tag: None, params: Params::new() };
            }
            Some(close) => {
                uri = slice(&s, open + 1, close);
                i = close + 1;
            }
        }
    } else {
        // addr-spec (bare URI).
        match index_of(&s, ';', i) {
            None => {
                let uri = slice(&s, i, len).trim().to_string();
                return ParsedNameAddr { display_name: None, uri, tag: None, params: Params::new() };
            }
            Some(semi) => {
                uri = slice(&s, i, semi).trim().to_string();
                i = semi;
            }
        }
    }

    let params = parse_header_params(&s, i);
    let tag = match params.get("tag") {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };

    ParsedNameAddr { display_name, uri, tag, params }
}

// ---------------------------------------------------------------------------
// Via parsing
// ---------------------------------------------------------------------------

pub fn parse_via(value: &str) -> ParsedVia {
    let s: Vec<char> = value.chars().collect();
    let mut i = skip_ws(&s, 0);

    // sent-protocol: "SIP/2.0/UDP" or "SIP / 2.0 / TCP".
    let proto_end = scan_until_one_of(&s, i, "/");
    let protocol = slice(&s, i, proto_end).trim().to_string();
    i = proto_end + 1;

    let ver_end = scan_until_one_of(&s, i, "/");
    let version = slice(&s, i, ver_end).trim().to_string();
    i = ver_end + 1;

    i = skip_ws(&s, i);
    let trans_end = scan_until_ws_or_semi(&s, i);
    let transport = slice(&s, i, trans_end).trim().to_string();
    i = trans_end;

    i = skip_ws(&s, i);
    let (host, port, host_end) = parse_host_port(&s, i);
    i = host_end;

    let params = parse_header_params(&s, i);
    let branch = match params.get("branch") {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };

    ParsedVia { protocol, version, transport, host, port, branch, params }
}

// ---------------------------------------------------------------------------
// Contact parsing
// ---------------------------------------------------------------------------

pub fn parse_contact(value: &str) -> ParsedContact {
    let parsed = parse_name_addr(value);
    ParsedContact { display_name: parsed.display_name, uri: parsed.uri, params: parsed.params }
}

// ---------------------------------------------------------------------------
// CSeq parsing: "number method"
// ---------------------------------------------------------------------------

pub fn parse_cseq(value: &str) -> ParsedCSeq {
    let s: Vec<char> = value.chars().collect();
    let mut i = skip_ws(&s, 0);
    let num_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let seq = fold_digits(&s, num_start, i);
    i = skip_ws(&s, i);
    let method = slice(&s, i, s.len()).trim().to_string();
    ParsedCSeq { seq, method }
}

// ---------------------------------------------------------------------------
// RAck parsing (RFC 3262 §7.2): "response-num CSeq-num method"
// ---------------------------------------------------------------------------

pub fn parse_rack(value: &str) -> Option<ParsedRack> {
    let s: Vec<char> = value.chars().collect();
    let mut i = skip_ws(&s, 0);

    let rseq_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == rseq_start {
        return None;
    }
    let rseq = fold_digits(&s, rseq_start, i);

    i = skip_ws(&s, i);
    if i >= s.len() {
        return None;
    }

    let seq_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == seq_start {
        return None;
    }
    let seq = fold_digits(&s, seq_start, i);

    i = skip_ws(&s, i);
    if i >= s.len() {
        return None;
    }

    let method = slice(&s, i, s.len()).trim().to_string();
    if method.is_empty() {
        return None;
    }
    if method.chars().any(|c| c == ' ' || c == '\t') {
        return None;
    }

    Some(ParsedRack { rseq, seq, method })
}

// ---------------------------------------------------------------------------
// Replaces parsing (RFC 3891 §6.1)
// ---------------------------------------------------------------------------

pub fn parse_replaces(value: &str) -> Option<ParsedReplaces> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let semi_idx = trimmed.find(';');
    let call_id = match semi_idx {
        None => trimmed.trim().to_string(),
        Some(idx) => trimmed[..idx].trim().to_string(),
    };
    if call_id.is_empty() {
        return None;
    }
    let semi_idx = match semi_idx {
        None => return None, // missing mandatory tags
        Some(idx) => idx,
    };

    let mut to_tag: Option<String> = None;
    let mut from_tag: Option<String> = None;
    let mut early_only = false;

    let param_str = &trimmed[semi_idx + 1..];
    for raw in param_str.split(';') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        match part.find('=') {
            None => {
                if part.to_lowercase() == "early-only" {
                    early_only = true;
                }
            }
            Some(eq) => {
                let k = part[..eq].trim().to_lowercase();
                let v = part[eq + 1..].trim().to_string();
                if k == "to-tag" {
                    to_tag = Some(v);
                } else if k == "from-tag" {
                    from_tag = Some(v);
                }
            }
        }
    }

    let to_tag = to_tag?;
    let from_tag = from_tag?;
    if to_tag.is_empty() || from_tag.is_empty() {
        return None;
    }

    Some(ParsedReplaces { call_id, to_tag, from_tag, early_only })
}

// ---------------------------------------------------------------------------
// Refer-To parsing (RFC 3515 §2.1, RFC 3891 §3 for embedded Replaces)
// ---------------------------------------------------------------------------

pub fn parse_refer_to(value: &str) -> Option<ParsedReferTo> {
    let name_addr = parse_name_addr(value);
    if name_addr.uri.is_empty() {
        return None;
    }

    let uri = name_addr.uri.clone();
    let uri_chars: Vec<char> = uri.chars().collect();
    let q_idx = find_uri_embedded_headers_start(&uri);

    let mut uri_head = uri.clone();
    let mut embedded_headers: BTreeMap<String, String> = BTreeMap::new();

    if let Some(q) = q_idx {
        uri_head = slice(&uri_chars, 0, q);
        let header_str = slice(&uri_chars, q + 1, uri_chars.len());
        for pair in header_str.split('&') {
            if pair.is_empty() {
                continue;
            }
            let eq = match pair.find('=') {
                Some(e) => e,
                None => continue,
            };
            let raw_key = &pair[..eq];
            let raw_val = &pair[eq + 1..];
            let (key, val) = match (decode_uri_component(raw_key), decode_uri_component(raw_val)) {
                (Ok(k), Ok(v)) => (k, v),
                _ => (raw_key.to_string(), raw_val.to_string()),
            };
            embedded_headers.insert(key, val);
        }
    }

    let parsed_uri = parse_sip_uri_string(&uri_head);

    let mut replaces: Option<ParsedReplaces> = None;
    for (k, v) in &embedded_headers {
        if k.to_lowercase() == "replaces" {
            replaces = parse_replaces(v);
            break;
        }
    }

    Some(ParsedReferTo {
        display_name: name_addr.display_name,
        uri,
        parsed_uri,
        params: name_addr.params,
        embedded_headers,
        replaces,
    })
}

// ---------------------------------------------------------------------------
// SIP URI parsing
// ---------------------------------------------------------------------------

/// Locate the `?` opening embedded URI-headers (RFC 3261 §19.1.1) — the one
/// after hostport, anchored past userinfo `@` so a userinfo `?` is not
/// misidentified. `None` when there are no embedded headers.
pub fn find_uri_embedded_headers_start(uri: &str) -> Option<usize> {
    let s: Vec<char> = uri.chars().collect();
    let colon_idx = index_of(&s, ':', 0)?;
    let mut at_idx: Option<usize> = None;
    for j in (colon_idx + 1)..s.len() {
        if s[j] == '@' {
            at_idx = Some(j);
            break;
        }
    }
    let host_start = match at_idx {
        None => colon_idx + 1,
        Some(a) => a + 1,
    };
    for j in host_start..s.len() {
        if s[j] == '?' {
            return Some(j);
        }
    }
    None
}

pub fn parse_sip_uri_string(uri: &str) -> Option<ParsedUri> {
    let s: Vec<char> = uri.chars().collect();
    let len = s.len();

    let colon_idx = index_of(&s, ':', 0)?;
    let scheme = slice(&s, 0, colon_idx).to_lowercase();
    let mut i = colon_idx + 1;

    let user: Option<String>;
    let host_start: usize;

    let at_idx = scan_until_one_of(&s, i, "@>");
    if at_idx < len && s[at_idx] == '@' {
        user = Some(slice(&s, i, at_idx));
        host_start = at_idx + 1;
    } else {
        user = None;
        host_start = i;
    }

    let (host, port, host_end) = parse_host_port(&s, host_start);
    i = host_end;

    let mut params: BTreeMap<String, String> = BTreeMap::new();
    while i < len && s[i] == ';' {
        i += 1;
        let name_end = scan_until_one_of(&s, i, "=;>? \t");
        let pname = slice(&s, i, name_end).to_lowercase();
        i = name_end;
        if i < len && s[i] == '=' {
            i += 1;
            let val_end = scan_until_one_of(&s, i, ";>? \t");
            params.insert(pname, slice(&s, i, val_end));
            i = val_end;
        } else {
            params.insert(pname, String::new());
        }
    }

    Some(ParsedUri { scheme, user, host, port, params })
}

// ---------------------------------------------------------------------------
// Internal helpers — index scanning over &[char]
// ---------------------------------------------------------------------------

fn slice(s: &[char], a: usize, b: usize) -> String {
    s[a.min(s.len())..b.min(s.len())].iter().collect()
}

fn index_of(s: &[char], needle: char, from: usize) -> Option<usize> {
    (from..s.len()).find(|&j| s[j] == needle)
}

fn skip_ws(s: &[char], mut i: usize) -> usize {
    while i < s.len() {
        let c = s[i];
        if c != ' ' && c != '\t' {
            break;
        }
        i += 1;
    }
    i
}

/// Read a quoted string starting at `i` (which must be `"`). Returns the
/// unescaped text and the position after the closing `"`.
fn read_quoted_string(s: &[char], mut i: usize) -> (String, usize) {
    i += 1; // skip opening "
    let mut result = String::new();
    while i < s.len() {
        let c = s[i];
        if c == '\\' && i + 1 < s.len() {
            result.push(s[i + 1]);
            i += 2;
            continue;
        }
        if c == '"' {
            return (result, i + 1);
        }
        result.push(s[i]);
        i += 1;
    }
    (result, i)
}

/// Scan forward until one of the delimiter chars; returns its index (or end).
fn scan_until_one_of(s: &[char], mut i: usize, delims: &str) -> usize {
    while i < s.len() {
        if delims.contains(s[i]) {
            return i;
        }
        i += 1;
    }
    i
}

/// Scan until whitespace, `;`, or `,`.
fn scan_until_ws_or_semi(s: &[char], mut i: usize) -> usize {
    while i < s.len() {
        let c = s[i];
        if c == ' ' || c == '\t' || c == ';' || c == ',' {
            return i;
        }
        i += 1;
    }
    i
}

fn fold_digits(s: &[char], from: usize, to: usize) -> u64 {
    let mut n: u64 = 0;
    for &c in &s[from..to] {
        if let Some(d) = c.to_digit(10) {
            n = n.saturating_mul(10).saturating_add(d as u64);
        }
    }
    n
}

/// Parse host[:port] from `i`. Host can be IPv4, bracketed IPv6, or hostname.
fn parse_host_port(s: &[char], i: usize) -> (String, Option<u64>, usize) {
    let len = s.len();

    // IPv6: [address]
    if i < len && s[i] == '[' {
        match index_of(s, ']', i + 1) {
            None => return (slice(s, i + 1, len), None, len),
            Some(close) => {
                let host = slice(s, i + 1, close);
                let mut j = close + 1;
                let mut port: Option<u64> = None;
                if j < len && s[j] == ':' {
                    j += 1;
                    let port_start = j;
                    while j < len && s[j].is_ascii_digit() {
                        j += 1;
                    }
                    // TS does not guard the IPv6 branch — empty digits yield NaN
                    // (an invalid port). We mirror with fold_digits → 0, which
                    // `is_valid_port` likewise rejects.
                    port = Some(fold_digits(s, port_start, j));
                }
                return (host, port, j);
            }
        }
    }

    // IPv4 or hostname: scan until : ; , > SP HTAB ?
    let host_end = scan_until_one_of(s, i, ":;,> \t?");
    let host = slice(s, i, host_end);

    let mut j = host_end;
    let mut port: Option<u64> = None;
    if j < len && s[j] == ':' {
        j += 1;
        let port_start = j;
        while j < len && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > port_start {
            port = Some(fold_digits(s, port_start, j));
        }
    }

    (host, port, j)
}

/// Parse header-level parameters (after `>` or after addr-spec). This is where
/// `tag=` lives — semicolon-separated `key[=value]` at the HEADER level.
fn parse_header_params(s: &[char], mut i: usize) -> Params {
    let mut params: Params = Params::new();
    let len = s.len();

    while i < len {
        i = skip_ws(s, i);
        if i >= len {
            break;
        }
        if s[i] != ';' {
            // Skip unexpected chars (commas for multi-value headers, etc.).
            i += 1;
            continue;
        }
        i += 1; // skip ;
        i = skip_ws(s, i);

        let name_end = scan_until_one_of(s, i, "=; \t,>");
        let pname = slice(s, i, name_end).to_lowercase();
        i = name_end;

        // RFC 3261 EQUAL permits surrounding LWS: `SWS "=" SWS`.
        i = skip_ws(s, i);
        if i < len && s[i] == '=' {
            i += 1;
            i = skip_ws(s, i);
            if i < len && s[i] == '"' {
                let (text, end) = read_quoted_string(s, i);
                params.insert(pname, ParamValue::Value(text));
                i = end;
            } else {
                let val_end = scan_until_one_of(s, i, ";, \t>");
                params.insert(pname, ParamValue::Value(slice(s, i, val_end)));
                i = val_end;
            }
        } else if !pname.is_empty() {
            params.insert(pname, ParamValue::Flag);
        }
    }

    params
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
                return Some(format!("IPv4 octet \"{label}\" has leading zero (octal-confusion vector)"));
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
    let s: Vec<char> = uri.chars().collect();
    if s.is_empty() {
        return Some("empty URI".to_string());
    }
    // Locate scheme colon.
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if c == ':' {
            break;
        }
        if i == 0 {
            if !c.is_ascii_alphabetic() {
                return Some("scheme must start with ALPHA".to_string());
            }
        } else {
            let is_alpha = c.is_ascii_alphabetic();
            let is_digit = c.is_ascii_digit();
            let is_special = c == '+' || c == '-' || c == '.';
            if !is_alpha && !is_digit && !is_special {
                return Some("invalid scheme character".to_string());
            }
        }
        i += 1;
    }
    if i >= s.len() {
        return Some("missing scheme colon".to_string());
    }
    let scheme = slice(&s, 0, i).to_lowercase();
    i += 1; // past ':'

    if scheme != "sip" && scheme != "sips" {
        return None;
    }

    // Locate the `@` terminating userinfo, if any.
    let mut at_idx: Option<usize> = None;
    let mut second_at = false;
    for j in i..s.len() {
        let c = s[j];
        if c == '@' {
            if at_idx.is_none() {
                at_idx = Some(j);
            } else {
                second_at = true;
                break;
            }
        } else if c == '>' {
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
    let first_host_char = s[host_start];
    if first_host_char == ':' {
        return Some("hostport starts with `:`".to_string());
    }
    if first_host_char == ';' || first_host_char == '?' || first_host_char == '>' {
        return Some("empty hostport".to_string());
    }

    // End of hostport: first `;`, `?`, or `>`.
    let mut host_end = s.len();
    for j in host_start..s.len() {
        let c = s[j];
        if c == ';' || c == '?' || c == '>' {
            host_end = j;
            break;
        }
    }

    if s[host_start] == '[' {
        // IPv6 reference: must close with `]`.
        let mut closed = false;
        let mut k = host_start + 1;
        while k < host_end {
            if s[k] == ']' {
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
            if s[after] != ':' {
                return Some("junk after `]` in hostport".to_string());
            }
            after += 1;
            if let Some(reason) = validate_port_digits(&s, after, host_end) {
                return Some(reason);
            }
        }
        return None;
    }

    // Plain host[:port]. Multiple unbracketed `:` = malformed.
    let mut colon_idx: Option<usize> = None;
    for j in host_start..host_end {
        if s[j] == ':' {
            if colon_idx.is_none() {
                colon_idx = Some(j);
            } else {
                return Some("multiple `:` in hostport".to_string());
            }
        }
    }
    let host = match colon_idx {
        None => slice(&s, host_start, host_end),
        Some(c) => slice(&s, host_start, c),
    };
    if host.is_empty() {
        return Some("empty host".to_string());
    }
    if let Some(reason) = validate_strict_host(&host) {
        return Some(reason);
    }
    if let Some(c) = colon_idx {
        if let Some(reason) = validate_port_digits(&s, c + 1, host_end) {
            return Some(reason);
        }
    }
    None
}

fn validate_port_digits(s: &[char], from: usize, to: usize) -> Option<String> {
    if from >= to {
        return Some("empty port".to_string());
    }
    let mut n: u64 = 0;
    for &c in &s[from..to] {
        if !c.is_ascii_digit() {
            return Some("non-digit in port".to_string());
        }
        n = n * 10 + c.to_digit(10).unwrap() as u64;
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
fn decode_uri_component(s: &str) -> Result<String, ()> {
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
