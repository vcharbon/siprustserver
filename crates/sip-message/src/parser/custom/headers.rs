//! SIP header-block parser. Port of `src/sip/parsers/custom/headers.ts`.
//! No regex. Handles folding, compact-form expansion, Content-Length
//! extraction, case-preserving names, and the registry-driven strict numeric
//! gate + CVE-regression quoted-string / Digest checks (ADR-0007).

use super::compact_forms::expand_compact_form;
use super::structured_headers::split_top_level_commas;
use super::scanner::{decode, is_token_char, is_wsp, strict_non_negative_decimal, Scanner, COLON, CR, HTAB, LF};
use crate::error::SipParseError;
use crate::parser::SipParserLimits;
use crate::types::SipHeader;

const UINT_32_MAX: u64 = (1u64 << 32) - 1;
const INT_32_MAX: u64 = (1u64 << 31) - 1;

struct NumericHeaderRule {
    max: u64,
    /// True if the value may carry a `;param=...` tail (RFC 4028 session timers).
    allow_param_tail: bool,
}

/// Numeric-header registry — every header whose value must be a non-negative
/// decimal integer within a defined range (see ADR-0007). `None` if not numeric.
/// Case-insensitive on the raw wire name (`eq_ignore_ascii_case`) so the caller
/// need not mint a lowercased `String` per header just to probe the registry.
fn numeric_header_rule(name: &str) -> Option<NumericHeaderRule> {
    let r = |max, allow_param_tail| Some(NumericHeaderRule { max, allow_param_tail });
    if name.eq_ignore_ascii_case("content-length") {
        r(INT_32_MAX, false)
    } else if name.eq_ignore_ascii_case("cseq") {
        r(INT_32_MAX, false)
    } else if name.eq_ignore_ascii_case("max-forwards") {
        r(255, false)
    } else if name.eq_ignore_ascii_case("expires") {
        r(UINT_32_MAX, false)
    } else if name.eq_ignore_ascii_case("min-expires") {
        r(UINT_32_MAX, false)
    } else if name.eq_ignore_ascii_case("session-expires") {
        r(UINT_32_MAX, true)
    } else if name.eq_ignore_ascii_case("min-se") {
        r(UINT_32_MAX, true)
    } else {
        None
    }
}

/// ASCII-case-insensitive membership against a set of already-lowercase
/// candidates — no allocation (contrast `name.to_lowercase()` then `match`).
fn eq_any_ignore_ascii_case(name: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|c| name.eq_ignore_ascii_case(c))
}

/// Strict numeric extraction with optional param tail. The digit prefix is
/// terminated at the first WSP (CSeq is `1*DIGIT LWS Method`).
fn extract_strict_numeric_prefix(value: &str, rule: &NumericHeaderRule) -> Option<u64> {
    let bytes = value.as_bytes();
    let mut end = bytes.len();
    if rule.allow_param_tail {
        if let Some(semi) = value.find(';') {
            end = semi;
        }
    }
    let mut i = 0;
    while i < end {
        let c = bytes[i];
        if c == 0x20 || c == 0x09 {
            break;
        }
        i += 1;
    }
    // `value` is already a valid `&str` and `i` lands on an ASCII WSP byte (or a
    // char boundary at `end`/len), so the prefix slice is valid — borrow it
    // rather than re-decoding into a fresh `String`.
    strict_non_negative_decimal(value[..i].trim(), rule.max)
}

pub struct ParsedHeaders {
    pub headers: Vec<SipHeader>,
    pub content_length: u64,
}

/// Parse all headers from the current position until the blank line, which is
/// consumed.
pub fn parse_headers(s: &mut Scanner, limits: &SipParserLimits) -> Result<ParsedHeaders, SipParseError> {
    let mut headers: Vec<SipHeader> = Vec::new();
    let mut content_length: u64 = 0;

    loop {
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
            break;
        }
        if s.pos >= s.buf.len() {
            break;
        }

        let raw_name = read_header_name(s)?;
        if raw_name.is_empty() {
            return Err(SipParseError::new(format!("Empty header name at position {}", s.pos)));
        }

        s.expect(COLON)?;
        s.skip_lws();

        let value = s.read_header_value();
        let name = expand_compact_form(&raw_name);
        let trimmed_value = trim_in_place(value);

        // Bound per-header memory: name + ": " + value, post-unfold/trim.
        let header_len = name.len() + 2 + trimmed_value.len();
        if header_len > limits.max_header_length {
            return Err(SipParseError::new(format!(
                "Header \"{name}\" length {header_len} exceeds limit {}",
                limits.max_header_length
            )));
        }

        // Registry-driven paranoid digit-only pass (ADR-0007).
        if let Some(rule) = numeric_header_rule(&name) {
            match extract_strict_numeric_prefix(&trimmed_value, &rule) {
                Some(parsed) => {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = parsed;
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
        if is_quoted_string_header(&name) && has_unbalanced_quotes(&trimmed_value) {
            return Err(SipParseError::new(format!("Unterminated quoted-string in {name} header")));
        }

        // Digest credentials must be comma-separated name=value pairs
        // (CVE-2023-28098).
        if is_authorization_header(&name) && !is_valid_digest_credentials(&trimmed_value) {
            return Err(SipParseError::new(format!("Malformed Digest credentials in {name} header")));
        }

        headers.push(SipHeader { name, value: trimmed_value });
    }

    Ok(ParsedHeaders { headers, content_length })
}

/// Trim surrounding whitespace in place — `read_header_value` already
/// allocated the value; `value.trim().to_string()` would mint a second copy
/// per header on the hot path. Same Unicode whitespace set as `str::trim`.
fn trim_in_place(mut value: String) -> String {
    let end = value.trim_end().len();
    value.truncate(end);
    let start = value.len() - value.trim_start().len();
    if start > 0 {
        value.drain(..start);
    }
    value
}

/// Headers whose RFC 3261 grammar can carry a quoted-string. Case-insensitive
/// on the raw wire name — no lowercased `String` allocation.
fn is_quoted_string_header(name: &str) -> bool {
    eq_any_ignore_ascii_case(
        name,
        &[
            "to",
            "from",
            "contact",
            "reply-to",
            "refer-to",
            "subject",
            "authorization",
            "proxy-authorization",
            "www-authenticate",
            "proxy-authenticate",
            "authentication-info",
            "warning",
            "alert-info",
            "call-info",
            "error-info",
        ],
    )
}

fn is_authorization_header(name: &str) -> bool {
    eq_any_ignore_ascii_case(name, &["authorization", "proxy-authorization"])
}

/// True iff `value` contains an unterminated quoted-string. Inside a quote,
/// `\<any>` is an escape pair. Operates on bytes (the relevant delimiters are
/// ASCII, never present in UTF-8 continuation bytes).
fn has_unbalanced_quotes(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_quote {
            if c == 0x5c {
                i += 2; // escape pair — skip next
                continue;
            }
            if c == 0x22 {
                in_quote = false;
            }
        } else if c == 0x22 {
            in_quote = true;
        }
        i += 1;
    }
    in_quote
}

/// Read the header name until colon, rejecting only unambiguously-illegal
/// bytes (CTL except HTAB, DEL) — preserves the RFC 4475 wide-range torture
/// test while catching CVE-2023-27598. Trailing WSP before the colon is
/// permitted (RFC 4475 §3.1.1.1).
fn read_header_name(s: &mut Scanner) -> Result<String, SipParseError> {
    let start = s.pos;
    while s.pos < s.buf.len() {
        let b = s.buf[s.pos];
        if b == COLON || b == CR || b == LF {
            break;
        }
        if is_wsp(b) {
            break;
        }
        if is_illegal_header_name_byte(b) {
            return Err(SipParseError::new(format!(
                "Illegal byte 0x{b:x} in header name at position {}",
                s.pos
            )));
        }
        s.pos += 1;
    }
    let name = decode(&s.buf[start..s.pos]);
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
    let params = value[SCHEME.len()..].trim_start();
    if params.is_empty() {
        return false;
    }

    for part in split_top_level_commas(params) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return false;
        }
        let eq_idx = match find_unquoted_equals(trimmed) {
            Some(i) => i,
            None => return false,
        };
        let name = trimmed[..eq_idx].trim_end();
        if name.is_empty() {
            return false;
        }
        if !name.bytes().all(is_token_char) {
            return false;
        }
    }
    true
}


/// Byte index of the first `=` outside a quoted-string, or `None`.
fn find_unquoted_equals(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_quote {
            if c == 0x5c {
                i += 2;
                continue;
            }
            if c == 0x22 {
                in_quote = false;
            }
        } else if c == 0x22 {
            in_quote = true;
        } else if c == 0x3d {
            return Some(i);
        }
        i += 1;
    }
    None
}
