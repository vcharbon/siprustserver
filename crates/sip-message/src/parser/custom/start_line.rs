//! SIP start-line parser — Request-Line and Status-Line. Port of
//! `src/sip/parsers/custom/start-line.ts`. No regex.
//!
//!   Request-Line:  Method SP Request-URI SP SIP-Version CRLF
//!   Status-Line:   SIP-Version SP Status-Code SP Reason-Phrase CRLF

use super::scanner::{span_text, Scanner, Span, CR, LF, SP};
use crate::char_class::is_token_char;
use crate::error::SipParseError;
use crate::parser::SipParserLimits;

/// The start line's elements as image spans — materialized by the caller,
/// which owns the image the spans index into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLine {
    pub method: Span,
    pub uri: Span,
    pub version: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub version: Span,
    pub status: u16,
    pub reason: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    Request(RequestLine),
    Status(StatusLine),
}

/// Parse the start line, advancing past the CRLF terminator. `image` is the
/// decoded message text the scanner walks — spans index into it.
pub fn parse_start_line(
    s: &mut Scanner,
    image: &str,
    limits: &SipParserLimits,
) -> Result<StartLine, SipParseError> {
    let first_token = read_until_sp(s);
    if first_token.is_empty() {
        return Err(SipParseError::new("Empty start line"));
    }

    // Exactly one SP between elements (RFC 3261 §7.1).
    if s.peek() != Some(SP) {
        return Err(SipParseError::new("Expected SP after first start-line token"));
    }
    s.advance();

    // Multiple SP in request line is invalid (3.1.2.9).
    if s.peek() == Some(SP) {
        return Err(SipParseError::new("Multiple SP in start line"));
    }

    if span_text(image, &first_token)?.starts_with("SIP/") {
        parse_status_line(s, image, first_token).map(StartLine::Status)
    } else {
        parse_request_line(s, image, first_token, limits).map(StartLine::Request)
    }
}

fn parse_request_line(
    s: &mut Scanner,
    image: &str,
    method: Span,
    limits: &SipParserLimits,
) -> Result<RequestLine, SipParseError> {
    // `Method = INVITEm / … / extension-method`, a token (§25.1).
    let method_text = span_text(image, &method)?;
    if limits.wire_grammar && !method_text.bytes().all(is_token_char) {
        return Err(SipParseError::new(format!("Method is not a token: {method_text:?}")));
    }

    // Request-URI must not be enclosed in angle brackets (3.1.2.7).
    if s.peek() == Some(0x3c) {
        return Err(SipParseError::new("Request-URI must not be enclosed in angle brackets"));
    }

    let uri = read_until_sp(s);
    if uri.is_empty() {
        return Err(SipParseError::new("Empty Request-URI"));
    }
    if uri.len() > limits.max_uri_length {
        return Err(SipParseError::new(format!(
            "Request-URI length {} exceeds limit {}",
            uri.len(),
            limits.max_uri_length
        )));
    }

    if s.peek() != Some(SP) {
        return Err(SipParseError::new("Expected SP after Request-URI"));
    }
    s.advance();

    // SIP-Version: rest until CRLF, no trailing whitespace (3.1.2.10).
    let version = read_until_crlf(s);
    let version_text = span_text(image, &version)?;
    if version_text != version_text.trim_end() {
        return Err(SipParseError::new("Trailing whitespace in request line"));
    }
    if version_text != "SIP/2.0" {
        return Err(SipParseError::new(format!("Unsupported SIP version: \"{version_text}\"")));
    }

    s.expect_crlf()?;

    Ok(RequestLine { method, uri, version })
}

fn parse_status_line(
    s: &mut Scanner,
    image: &str,
    version: Span,
) -> Result<StatusLine, SipParseError> {
    let version_text = span_text(image, &version)?;
    if version_text != "SIP/2.0" {
        return Err(SipParseError::new(format!("Unsupported SIP version: \"{version_text}\"")));
    }

    // Status-Code: exactly 3 digits.
    let status_span = read_until_sp(s);
    let status_str = span_text(image, &status_span)?;
    let status = match js_parse_int_base10(status_str) {
        Some(status) if status_str.len() == 3 => status,
        _ => return Err(SipParseError::new(format!("Invalid status code: \"{status_str}\""))),
    };
    let status = match u16::try_from(status) {
        Ok(status) if (100..=699).contains(&status) => status,
        _ => return Err(SipParseError::new(format!("Status code out of range: {status}"))),
    };

    // SP before reason phrase, but the reason may be empty (3.1.1.13) so the
    // SP is optional in that case.
    s.skip_if(SP);

    let reason = read_until_crlf(s);
    s.expect_crlf()?;

    Ok(StatusLine { version, status, reason })
}

/// Read bytes until SP (or CR/LF for empty-reason edge cases), without
/// consuming the delimiter.
fn read_until_sp(s: &mut Scanner) -> Span {
    s.take_while(|b| b != SP && b != CR && b != LF)
}

/// Read bytes until CRLF (or bare LF), without consuming the line ending.
fn read_until_crlf(s: &mut Scanner) -> Span {
    s.take_while(|b| b != CR && b != LF)
}

/// Mimic JS `parseInt(s, 10)`: optional leading ASCII whitespace, optional
/// sign, then a run of decimal digits; trailing garbage ignored; no digits →
/// `None` (NaN). Reproduces the status-code validation exactly.
fn js_parse_int_base10(s: &str) -> Option<i64> {
    let s = s.trim_start_matches([' ', '\t']);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..)?),
        Some(b'+') => (false, s.get(1..)?),
        _ => (false, s),
    };
    let mut digits = s.bytes().map_while(|b| char::from(b).to_digit(10)).peekable();
    digits.peek()?;
    let n = digits.fold(0i64, |n, d| n.saturating_mul(10).saturating_add(i64::from(d)));
    Some(if neg { -n } else { n })
}
