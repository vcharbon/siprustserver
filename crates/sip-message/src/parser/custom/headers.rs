//! SIP header-block parser. No regex. Handles folding, compact-form
//! expansion, Content-Length extraction and case-preserving names. Each value
//! meets the grammar's control-byte rule (one only as a quoted-pair's escaped
//! byte) and its singleton rule for the headers the split reads (one
//! Content-Length, one Max-Forwards), then the gates beyond the grammar
//! (ADR-0001): the registry-driven strict numeric values and the
//! CVE-regression quoted-string / Digest checks.

use super::compact_forms::expand_compact_form;
use super::scanner::{
    is_wsp, strict_non_negative_decimal, trim_span, HeaderValue, Scanner, Span, COLON, CR, HTAB, LF,
};
use super::structured_headers::split_top_level_commas;
use crate::char_class::is_token_char;
use crate::control_bytes::{bare_control_byte, has_comments};
use crate::error::SipParseError;
use crate::header::{HeaderId, HeaderName};
use crate::header_table::{HeaderTable, Ids};
use crate::parser::SipParserLimits;
use crate::sip_str::{SharedText, SipStr};
use crate::types::SipHeader;

const UINT_32_MAX: u64 = (1u64 << 32) - 1;
const INT_32_MAX: u64 = (1u64 << 31) - 1;

struct NumericHeaderRule {
    max: u64,
    tail: NumericTail,
}

/// What a numeric header's grammar has after its digits.
enum NumericTail {
    /// Nothing: the value is `1*DIGIT`.
    None,
    /// `*( SEMI param )` (RFC 4028 session timers).
    Params,
    /// `LWS Method` (CSeq), read by the CSeq reader.
    Method,
}

/// Numeric-header registry — every header whose value is a non-negative
/// decimal integer within a defined range (ADR-0001, numeric headers). `None`
/// if not numeric.
fn numeric_header_rule(name: &HeaderName) -> Option<NumericHeaderRule> {
    let r = |max, tail| Some(NumericHeaderRule { max, tail });
    match name {
        HeaderName::ContentLength => r(INT_32_MAX, NumericTail::None),
        HeaderName::CSeq => r(INT_32_MAX, NumericTail::Method),
        HeaderName::MaxForwards => r(255, NumericTail::None),
        HeaderName::Expires | HeaderName::MinExpires => r(UINT_32_MAX, NumericTail::None),
        HeaderName::SessionExpires | HeaderName::MinSe => r(UINT_32_MAX, NumericTail::Params),
        _ => None,
    }
}

/// The number a trimmed numeric value states, `None` unless its digits are
/// followed by nothing but the tail its grammar has.
fn strict_numeric_value(value: &str, rule: &NumericHeaderRule) -> Option<u64> {
    let digits = match rule.tail {
        NumericTail::None => value,
        // `SEMI = SWS ";" SWS`: whitespace may stand before the `;`.
        NumericTail::Params => value.split_once(';').map_or(value, |(head, _)| head.trim_end()),
        NumericTail::Method => value.split_once([' ', '\t']).map_or(value, |(head, _)| head),
    };
    strict_non_negative_decimal(digits, rule.max)
}

/// The headers whose repeat the split refuses: Content-Length and
/// Max-Forwards (RFC 3261 §7.3.1: a header whose grammar is no comma list
/// appears once). A repeated From, To, Call-ID or CSeq is refused at
/// extraction (`extract_fields`); the split checks no other repeat.
#[derive(Default)]
struct Singletons {
    content_length: Option<u64>,
    max_forwards: bool,
}

pub struct ParsedHeaders {
    /// The lines that passed, in wire order, each with the header it was
    /// classified as.
    pub headers: HeaderTable,
    /// The declared `Content-Length`; `None` when the block states none, which
    /// the transport framing then resolves (RFC 3261 §18.3).
    pub content_length: Option<u64>,
    /// The first line that broke the grammar or a gate; the split read past it.
    pub refusal: Option<SipParseError>,
    /// Whether a refused line named Via, From, To, Call-ID or CSeq, or had a
    /// name that did not read: a field a response echoes may then be missing
    /// from `headers`.
    pub echoed_line_refused: bool,
}

/// Whether `name` is one of the fields a response echoes (RFC 3261 §8.2.6.2).
fn is_echoed(name: &HeaderName) -> bool {
    matches!(
        name,
        HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq
    )
}

/// How one header line ended.
enum Line {
    /// It passed, and is in the list.
    Kept,
    /// It broke the grammar or a gate; the scanner is past it. `echoed` when
    /// it named a field a response echoes, or its name did not read.
    Refused { error: SipParseError, echoed: bool },
}

/// Scan the ordered **as-wire** header names from a raw datagram — RFC 3261
/// §7.3.3 compact forms are NOT expanded (`v` stays `v`). Mirrors
/// [`parse_headers`]'s line loop but reads names only (no value validation), so
/// name-scanning stays a single source of truth. Returns the names in wire
/// order aligned 1:1 with [`parse_headers`]'s `headers`; an empty Vec when the
/// head is not UTF-8 or its start line or any header line is malformed (the
/// caller falls back to canonical names). Used to recover the captured
/// name-form for near-verbatim replay after the parser has already
/// canonicalized `SipHeader.name`.
pub fn scan_header_name_forms(raw: &[u8], limits: &SipParserLimits) -> Vec<String> {
    let block = super::scanner::header_block(raw);
    if block.bare_break.is_some() {
        return Vec::new();
    }
    let Some(image) = raw.get(..block.end).and_then(|head| std::str::from_utf8(head).ok()) else {
        return Vec::new();
    };
    let mut s = Scanner::new(image.as_bytes());
    if super::start_line::parse_start_line(&mut s, image, limits).is_err() {
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
///
/// A line that breaks the grammar or a gate is left out and the split reads
/// on, the first such line kept as the refusal. A line past the length limit
/// stops the split: the `Err` is that first refused line, else the limit.
pub fn parse_headers(
    s: &mut Scanner,
    text: &SharedText,
    lines: usize,
    limits: &SipParserLimits,
) -> Result<ParsedHeaders, SipParseError> {
    // Size the list up front from the block's line count — growing it from
    // empty cost one reallocation per doubling, several per message.
    let mut kept = Kept { lines: Vec::with_capacity(lines), ids: Ids::with_capacity(lines) };
    let mut refusal: Option<SipParseError> = None;
    let mut echoed_line_refused = false;
    let mut singletons = Singletons::default();

    loop {
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
            break;
        }
        if s.at_end() {
            break;
        }
        match parse_line(s, text, limits, &mut kept, &mut singletons) {
            Ok(Line::Kept) => {}
            Ok(Line::Refused { error, echoed }) => {
                echoed_line_refused |= echoed;
                refusal.get_or_insert(error);
            }
            Err(limit) => return Err(refusal.unwrap_or(limit)),
        }
    }

    Ok(ParsedHeaders {
        headers: HeaderTable::classified(kept.lines, kept.ids),
        content_length: singletons.content_length,
        refusal,
        echoed_line_refused,
    })
}

/// The lines kept so far and the header each was classified as.
struct Kept {
    lines: Vec<SipHeader>,
    ids: Ids,
}

/// The rest of a refused line consumed, folds included, so the split resumes
/// at the next line. `echoed` as [`Line::Refused`] states it.
fn refused(s: &mut Scanner, error: SipParseError, echoed: bool) -> Result<Line, SipParseError> {
    s.read_header_value()?;
    Ok(Line::Refused { error, echoed })
}

/// Parse the header line at the scanner, pushing it to `parsed` when it
/// passes. `Err` is a limit, which stops the split.
fn parse_line(
    s: &mut Scanner,
    text: &SharedText,
    limits: &SipParserLimits,
    kept: &mut Kept,
    singletons: &mut Singletons,
) -> Result<Line, SipParseError> {
    let image = text.as_str();
    // A name that does not read may have been any header, the five a response
    // echoes included.
    let raw_name = match read_header_name(s) {
        Ok(raw_name) => raw_name,
        Err(error) => return refused(s, error, true),
    };
    if raw_name.is_empty() {
        let error = SipParseError::new(format!("Empty header name at position {}", s.pos()));
        return refused(s, error, true);
    }
    let name_bytes = image.as_bytes().get(raw_name.clone());
    if limits.wire_grammar && !name_bytes.is_some_and(|n| n.iter().all(|&b| is_token_char(b))) {
        let error = SipParseError::new(format!(
            "Header name is not a token at position {}",
            raw_name.start
        ));
        return refused(s, error, true);
    }

    let name = expand_compact_form(text, raw_name);
    // Header identity is resolved once, here, from bytes the scanner has just
    // walked; every gate below dispatches on it instead of re-probing the name
    // against a candidate list.
    let id = HeaderId::of(&name);
    let known = id.map(HeaderId::name);
    if let Err(error) = s.expect(COLON) {
        return refused(s, error, known.as_ref().is_some_and(is_echoed));
    }
    s.skip_lws();

    let value = s.read_header_value()?;
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

    let echoed = known.as_ref().is_some_and(is_echoed);
    let comments = known.as_ref().is_some_and(has_comments);
    if let Some(at) = bare_control_byte(trimmed_value.as_bytes(), comments) {
        let byte = trimmed_value.as_bytes().get(at).copied().unwrap_or_default();
        let error = SipParseError::new(format!(
            "Header \"{name}\" holds control byte 0x{byte:02x} outside a quoted-pair \
             (RFC 3261 §25.1)"
        ));
        return Ok(Line::Refused { error, echoed });
    }
    if let Some(known) = known {
        if let Some(error) = gate_refusal(&known, &name, &trimmed_value, singletons) {
            return Ok(Line::Refused { error, echoed });
        }
    }

    kept.lines.push(SipHeader { name, value: trimmed_value });
    kept.ids.push(id);
    Ok(Line::Kept)
}

/// The refusal the value of the known header `known` draws, if any: a
/// repeated singleton, or a gate beyond the grammar (ADR-0001). A
/// Content-Length or Max-Forwards that passes is recorded in `singletons`,
/// which refuses a second one.
fn gate_refusal(
    known: &HeaderName,
    name: &SipStr,
    value: &SipStr,
    singletons: &mut Singletons,
) -> Option<SipParseError> {
    let repeated = match known {
        HeaderName::ContentLength => singletons.content_length.is_some(),
        HeaderName::MaxForwards => singletons.max_forwards,
        _ => false,
    };
    if repeated {
        return Some(SipParseError::new(format!(
            "Multiple {} headers (RFC 3261 §7.3.1 — one value)",
            known.as_wire_str()
        )));
    }

    // Registry-driven paranoid digit-only pass.
    if let Some(rule) = numeric_header_rule(known) {
        let Some(parsed) = strict_numeric_value(value, &rule) else {
            return Some(SipParseError::new(format!("Invalid {name} numeric value: \"{value}\"")));
        };
        match known {
            HeaderName::ContentLength => singletons.content_length = Some(parsed),
            HeaderName::MaxForwards => singletons.max_forwards = true,
            _ => {}
        }
    }

    // Reject unterminated quoted-strings on quoted-string-bearing headers
    // (CVE-2023-27599).
    if is_quoted_string_header(known) && has_unbalanced_quotes(value) {
        return Some(SipParseError::new(format!("Unterminated quoted-string in {name} header")));
    }

    // Digest credentials must be comma-separated name=value pairs
    // (CVE-2023-28098).
    if is_authorization_header(known) && !is_valid_digest_credentials(value) {
        return Some(SipParseError::new(format!("Malformed Digest credentials in {name} header")));
    }
    None
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
