//! SIP start-line parser — Request-Line and Status-Line. Port of
//! `src/sip/parsers/custom/start-line.ts`. No regex.
//!
//!   Request-Line:  Method SP Request-URI SP SIP-Version CRLF
//!   Status-Line:   SIP-Version SP Status-Code SP Reason-Phrase CRLF

use super::scanner::{Scanner, CR, LF, SP};
use crate::error::SipParseError;
use crate::parser::SipParserLimits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLine {
    pub method: String,
    pub uri: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub version: String,
    pub status: u16,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartLine {
    Request(RequestLine),
    Status(StatusLine),
}

/// Parse the start line, advancing past the CRLF terminator.
pub fn parse_start_line(s: &mut Scanner, limits: &SipParserLimits) -> Result<StartLine, SipParseError> {
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

    if first_token.starts_with("SIP/") {
        parse_status_line(s, first_token).map(StartLine::Status)
    } else {
        parse_request_line(s, first_token, limits).map(StartLine::Request)
    }
}

fn parse_request_line(
    s: &mut Scanner,
    method: String,
    limits: &SipParserLimits,
) -> Result<RequestLine, SipParseError> {
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
    if version != version.trim_end() {
        return Err(SipParseError::new("Trailing whitespace in request line"));
    }
    if version != "SIP/2.0" {
        return Err(SipParseError::new(format!("Unsupported SIP version: \"{version}\"")));
    }

    s.expect_crlf()?;

    Ok(RequestLine { method: method.to_uppercase(), uri, version })
}

fn parse_status_line(s: &mut Scanner, version: String) -> Result<StatusLine, SipParseError> {
    if version != "SIP/2.0" {
        return Err(SipParseError::new(format!("Unsupported SIP version: \"{version}\"")));
    }

    // Status-Code: exactly 3 digits.
    let status_str = read_until_sp(s);
    let status = js_parse_int_base10(&status_str);
    if status.is_none() || status_str.len() != 3 {
        return Err(SipParseError::new(format!("Invalid status code: \"{status_str}\"")));
    }
    let status = status.unwrap();
    if !(100..=699).contains(&status) {
        return Err(SipParseError::new(format!("Status code out of range: {status}")));
    }

    // SP before reason phrase, but the reason may be empty (3.1.1.13) so the
    // SP is optional in that case.
    if s.peek() == Some(SP) {
        s.advance();
    }

    let reason = read_until_crlf(s);
    s.expect_crlf()?;

    Ok(StatusLine { version, status: status as u16, reason })
}

/// Read bytes until SP (or CR/LF for empty-reason edge cases), without
/// consuming the delimiter.
fn read_until_sp(s: &mut Scanner) -> String {
    let start = s.pos;
    while s.pos < s.buf.len() {
        let b = s.buf[s.pos];
        if b == SP || b == CR || b == LF {
            break;
        }
        s.pos += 1;
    }
    String::from_utf8_lossy(&s.buf[start..s.pos]).into_owned()
}

/// Read bytes until CRLF (or bare LF), without consuming the line ending.
fn read_until_crlf(s: &mut Scanner) -> String {
    let start = s.pos;
    while s.pos < s.buf.len() {
        let b = s.buf[s.pos];
        if b == CR || b == LF {
            break;
        }
        s.pos += 1;
    }
    String::from_utf8_lossy(&s.buf[start..s.pos]).into_owned()
}

/// Mimic JS `parseInt(s, 10)`: optional leading ASCII whitespace, optional
/// sign, then a run of decimal digits; trailing garbage ignored; no digits →
/// `None` (NaN). Reproduces the status-code validation exactly.
fn js_parse_int_base10(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let dstart = i;
    let mut n: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        n = n.saturating_mul(10).saturating_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    if i == dstart {
        return None;
    }
    Some(if neg { -n } else { n })
}
