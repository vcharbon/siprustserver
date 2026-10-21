//! SIP header-block parser. Port of `src/sip/parsers/custom/headers.ts`.
//! No regex. Handles folding, compact-form expansion, Content-Length
//! extraction, case-preserving names, and the registry-driven strict numeric
//! gate + CVE-regression quoted-string / Digest checks (the gates beyond the
//! grammar are listed in ADR-0001).

use super::compact_forms::expand_compact_form;
use super::scanner::{
    decode, is_wsp, strict_non_negative_decimal, trim_span, HeaderValue, Scanner, Span, COLON, CR,
    HTAB, LF,
};
use super::structured_headers::split_top_level_commas;
use crate::char_class::is_token_char;
use crate::error::SipParseError;
use crate::header::HeaderName;
use crate::parser::SipParserLimits;
use crate::sip_str::{SharedText, SipStr};
use crate::types::SipHeader;

const UINT_32_MAX: u64 = (1u64 << 32) - 1;
const INT_32_MAX: u64 = (1u64 << 31) - 1;

struct NumericHeaderRule {
    max: u64,
    /// True if the value may carry a `;param=...` tail (RFC 4028 session timers).
    allow_param_tail: bool,
}

/// Numeric-header registry — every header whose value must be a non-negative
/// decimal integer within a defined range (ADR-0001, numeric headers). `None`
/// if not numeric.
fn numeric_header_rule(name: &HeaderName) -> Option<NumericHeaderRule> {
    let r = |max, allow_param_tail| Some(NumericHeaderRule { max, allow_param_tail });
    match name {
        HeaderName::ContentLength | HeaderName::CSeq => r(INT_32_MAX, false),
        HeaderName::MaxForwards => r(255, false),
        HeaderName::Expires | HeaderName::MinExpires => r(UINT_32_MAX, false),
        HeaderName::SessionExpires | HeaderName::MinSe => r(UINT_32_MAX, true),
        _ => None,
    }
}

/// Strict numeric extraction with optional param tail. The digit prefix is
/// terminated at the first WSP (CSeq is `1*DIGIT LWS Method`).
fn extract_strict_numeric_prefix(value: &str, rule: &NumericHeaderRule) -> Option<u64> {
    let scoped = match value.split_once(';') {
        Some((head, _)) if rule.allow_param_tail => head,
        _ => value,
    };
    let digits = match scoped.find([' ', '\t']) {
        Some(wsp) => scoped.get(..wsp)?,
        None => scoped,
    };
    strict_non_negative_decimal(digits.trim(), rule.max)
}

pub struct ParsedHeaders {
    pub headers: Vec<SipHeader>,
    /// The declared `Content-Length`; `None` when the block states none, which
    /// the transport framing then resolves (RFC 3261 §18.3).
    pub content_length: Option<u64>,
}

/// Scan the ordered **as-wire** header names from a raw datagram — RFC 3261
/// §7.3.3 compact forms are NOT expanded (`v` stays `v`). Mirrors
/// [`parse_headers`]'s line loop but reads names only (no value validation), so
/// name-scanning stays a single source of truth. Returns the names in wire
/// order aligned 1:1 with [`parse_headers`]'s `headers`; an empty Vec when the
/// start line or any header line is malformed (the caller falls back to
/// canonical names). Used to recover the captured name-form for near-verbatim
/// replay after the parser has already canonicalized `SipHeader.name`.
pub fn scan_header_name_forms(raw: &[u8], limits: &SipParserLimits) -> Vec<String> {
    let image = decode(raw);
    let mut s = Scanner::new(image.as_bytes());
    if super::start_line::parse_start_line(&mut s, &image, limits).is_err() {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    loop {
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
            break;
        }
        if s.at_end() {
            break;
        }
        let raw_name = match read_header_name(&mut s) {
            Ok(n) if !n.is_empty() => n,
            _ => return Vec::new(),
        };
        if s.expect(COLON).is_err() {
            return Vec::new();
        }
        s.skip_lws();
        let (Ok(_), Some(name)) = (s.read_header_value(), image.get(raw_name)) else {
            return Vec::new();
        };
        names.push(name.to_string());
    }
    names
}

/// Parse all headers from the current position until the blank line, which is
/// consumed. `text` is the message image the scanner walks; every name/value
/// comes back as a span of it, so a header costs no allocation.
pub fn parse_headers(
    s: &mut Scanner,
    text: &SharedText,
    lines: usize,
    limits: &SipParserLimits,
) -> Result<ParsedHeaders, SipParseError> {
    let image = text.as_str();
    // Size the list up front from the block's line count — growing it from
    // empty cost one reallocation per doubling, several per message.
    let mut headers: Vec<SipHeader> = Vec::with_capacity(lines);
    let mut content_length: Option<u64> = None;

    loop {
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
            break;
        }
        if s.at_end() {
            break;
        }

        let raw_name = read_header_name(s)?;
        if raw_name.is_empty() {
            return Err(SipParseError::new(format!("Empty header name at position {}", s.pos())));
        }
        let name_bytes = image.as_bytes().get(raw_name.clone());
        if limits.wire_grammar && !name_bytes.is_some_and(|n| n.iter().all(|&b| is_token_char(b))) {
            return Err(SipParseError::new(format!(
                "Header name is not a token at position {}",
                raw_name.start
            )));
        }

        s.expect(COLON)?;
        s.skip_lws();

        let value = s.read_header_value()?;
        let name = expand_compact_form(text, raw_name);
        let trimmed_value = match value {
            HeaderValue::Span(range) => {
                let t = trim_span(image, range.clone()).ok_or_else(|| {
                    SipParseError::new(format!("header value {range:?} outside the image"))
                })?;
                text.span(t.start, t.len())
            }
            HeaderValue::Unfolded(v) => SipStr::owned(v.trim()),
        };

        // Bound per-header memory: name + ": " + value, post-unfold/trim.
        let header_len = name.len() + 2 + trimmed_value.len();
        if header_len > limits.max_header_length {
            return Err(SipParseError::new(format!(
                "Header \"{name}\" length {header_len} exceeds limit {}",
                limits.max_header_length
            )));
        }

        // Header identity is resolved once, here, from bytes the scanner has
        // just walked; every gate below dispatches on it instead of re-probing
        // the name against a candidate list.
        if let Some(known) = HeaderName::known(&name) {
            // Registry-driven paranoid digit-only pass.
            if let Some(rule) = numeric_header_rule(&known) {
                match extract_strict_numeric_prefix(&trimmed_value, &rule) {
                    Some(parsed) => {
                        if known == HeaderName::ContentLength {
                            content_length = Some(parsed);
                        }
                    }
                    None => {
                        return Err(SipParseError::new(format!(
                            "Invalid {name} numeric value: \"{trimmed_value}\""
                        )));
                    }
                }
            }

            // Reject unterminated quoted-strings on quoted-string-bearing headers
            // (CVE-2023-27599).
            if is_quoted_string_header(&known) && has_unbalanced_quotes(&trimmed_value) {
                return Err(SipParseError::new(format!(
                    "Unterminated quoted-string in {name} header"
                )));
            }

            // Digest credentials must be comma-separated name=value pairs
            // (CVE-2023-28098).
            if is_authorization_header(&known) && !is_valid_digest_credentials(&trimmed_value) {
                return Err(SipParseError::new(format!(
                    "Malformed Digest credentials in {name} header"
                )));
            }
        }

        headers.push(SipHeader { name, value: trimmed_value });
    }

    Ok(ParsedHeaders { headers, content_length })
}

/// Headers whose RFC 3261 grammar can carry a quoted-string.
fn is_quoted_string_header(name: &HeaderName) -> bool {
    matches!(
        name,
        HeaderName::To
            | HeaderName::From
            | HeaderName::Contact
            | HeaderName::ReplyTo
            | HeaderName::ReferTo
            | HeaderName::Subject
            | HeaderName::Authorization
            | HeaderName::ProxyAuthorization
            | HeaderName::WwwAuthenticate
            | HeaderName::ProxyAuthenticate
            | HeaderName::AuthenticationInfo
            | HeaderName::Warning
            | HeaderName::AlertInfo
            | HeaderName::CallInfo
            | HeaderName::ErrorInfo
    )
}

fn is_authorization_header(name: &HeaderName) -> bool {
    matches!(name, HeaderName::Authorization | HeaderName::ProxyAuthorization)
}

/// True iff `value` contains an unterminated quoted-string. Inside a quote,
/// `\<any>` is an escape pair. Operates on bytes (the relevant delimiters are
/// ASCII, never present in UTF-8 continuation bytes).
fn has_unbalanced_quotes(value: &str) -> bool {
    let mut bytes = value.bytes();
    let mut in_quote = false;
    while let Some(c) = bytes.next() {
        if in_quote {
            if c == 0x5c {
                bytes.next(); // escape pair — skip next
                continue;
            }
            if c == 0x22 {
                in_quote = false;
            }
        } else if c == 0x22 {
            in_quote = true;
        }
    }
    in_quote
}

/// Read the header name until colon, rejecting only unambiguously-illegal
/// bytes (CTL except HTAB, DEL) — preserves the RFC 4475 wide-range torture
/// test while catching CVE-2023-27598. Trailing WSP before the colon is
/// permitted (RFC 4475 §3.1.1.1).
fn read_header_name(s: &mut Scanner) -> Result<Span, SipParseError> {
    let start = s.pos();
    while let Some(b) = s.peek() {
        if b == COLON || b == CR || b == LF || is_wsp(b) {
            break;
        }
        if is_illegal_header_name_byte(b) {
            return Err(SipParseError::new(format!(
                "Illegal byte 0x{b:x} in header name at position {}",
                s.pos()
            )));
        }
        s.advance();
    }
    let name = start..s.pos();
    s.skip_wsp();
    Ok(name)
}

/// CTL bytes (0x00-0x1F except HTAB) and DEL (0x7F) never belong in a name.
fn is_illegal_header_name_byte(b: u8) -> bool {
    if b == HTAB {
        return false;
    }
    b < 0x20 || b == 0x7f
}

/// Validate Authorization / Proxy-Authorization. Only `Digest` is grammar-
/// checked; other schemes pass through. Digest must be 1+ comma-separated
/// `name=value` pairs with a non-empty token name (RFC 3261 §22.4).
fn is_valid_digest_credentials(value: &str) -> bool {
    const SCHEME: &str = "digest";
    // ASCII-case-insensitive prefix probe on the bytes — no lowercased copy of
    // the whole (credential-length) value just to read the scheme token.
    let is_digest_scheme = value
        .as_bytes()
        .get(..SCHEME.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(SCHEME.as_bytes()));
    if !is_digest_scheme {
        return true; // not Digest; skip
    }
    let after_scheme = value.as_bytes().get(SCHEME.len()).copied();
    if let Some(b) = after_scheme {
        if !is_wsp(b) {
            return true; // e.g. "Digestion" — not the Digest scheme
        }
    }
    // The scheme probe read ASCII bytes, so its end is a char boundary.
    let Some(params) = value.get(SCHEME.len()..).map(str::trim_start) else {
        return false;
    };
    if params.is_empty() {
        return false;
    }

    for part in split_top_level_commas(params) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(name) = name_before_unquoted_equals(trimmed) else {
            return false;
        };
        let name = name.trim_end();
        if name.is_empty() {
            return false;
        }
        if !name.bytes().all(is_token_char) {
            return false;
        }
    }
    true
}

/// The text before the first `=` outside a quoted-string, or `None`.
fn name_before_unquoted_equals(s: &str) -> Option<&str> {
    let mut bytes = s.bytes().enumerate();
    let mut in_quote = false;
    while let Some((i, c)) = bytes.next() {
        if in_quote {
            if c == 0x5c {
                bytes.next();
                continue;
            }
            if c == 0x22 {
                in_quote = false;
            }
        } else if c == 0x22 {
            in_quote = true;
        } else if c == 0x3d {
            return s.get(..i);
        }
    }
    None
}
