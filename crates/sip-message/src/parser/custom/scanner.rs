//! Byte-level cursor scanner. Port of `src/sip/parsers/custom/scanner.ts`.
//! All character classification is direct byte comparison — zero regex.
//!
//! TS used `throw` as unwinding control flow, caught at the parser entry
//! point. The Rust port surfaces those as `Result<_, SipParseError>` on the
//! methods that threw (`read_digits`, `expect`, `expect_crlf`); the rest are
//! infallible, matching the TS signatures.

use crate::error::SipParseError;

// ASCII byte constants.
pub const SP: u8 = 0x20; // space
pub const HTAB: u8 = 0x09; // horizontal tab
pub const CR: u8 = 0x0d; // \r
pub const LF: u8 = 0x0a; // \n
pub const COLON: u8 = 0x3a; // :

/// RFC 3261 token character: alphanum + `-.!%*_+\`'~`.
pub fn is_token_char(b: u8) -> bool {
    matches!(b,
        0x41..=0x5a // A-Z
        | 0x61..=0x7a // a-z
        | 0x30..=0x39 // 0-9
        | 0x2d // -
        | 0x2e // .
        | 0x21 // !
        | 0x25 // %
        | 0x2a // *
        | 0x5f // _
        | 0x2b // +
        | 0x60 // `
        | 0x27 // '
        | 0x7e // ~
    )
}

pub fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

pub fn is_wsp(b: u8) -> bool {
    b == SP || b == HTAB
}

/// Lenient UTF-8 decode of a byte range, matching Node `Buffer.toString("utf-8")`
/// (invalid sequences → U+FFFD).
fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub struct Scanner<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn at(buf: &'a [u8], pos: usize) -> Self {
        Self { buf, pos }
    }

    /// Peek at the current byte without advancing. `None` at end.
    pub fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// Read the current byte and advance. `None` at end.
    pub fn advance(&mut self) -> Option<u8> {
        let b = self.buf.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    /// Skip over SP and HTAB.
    pub fn skip_wsp(&mut self) {
        while self.pos < self.buf.len() && is_wsp(self.buf[self.pos]) {
            self.pos += 1;
        }
    }

    /// Skip LWS: SP/HTAB and header-folding (CRLF followed by SP/HTAB).
    pub fn skip_lws(&mut self) {
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            if is_wsp(b) {
                self.pos += 1;
                continue;
            }
            if b == CR
                && self.pos + 2 < self.buf.len()
                && self.buf[self.pos + 1] == LF
                && is_wsp(self.buf[self.pos + 2])
            {
                self.pos += 3; // skip CR LF WSP
                continue;
            }
            break;
        }
    }

    /// Read bytes until the given delimiter byte. Does not consume it.
    pub fn read_until(&mut self, byte: u8) -> String {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != byte {
            self.pos += 1;
        }
        decode(&self.buf[start..self.pos])
    }

    /// Read an RFC 3261 token (alphanum + special chars).
    pub fn read_token(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.buf.len() && is_token_char(self.buf[self.pos]) {
            self.pos += 1;
        }
        decode(&self.buf[start..self.pos])
    }

    /// Read 1+ digits as an integer. Errors if no digits found.
    pub fn read_digits(&mut self) -> Result<u64, SipParseError> {
        let start = self.pos;
        while self.pos < self.buf.len() && is_digit(self.buf[self.pos]) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(SipParseError::new(format!("Expected digit at position {}", self.pos)));
        }
        // ASCII digits only in range → valid UTF-8, safe to parse.
        decode(&self.buf[start..self.pos])
            .parse::<u64>()
            .map_err(|_| SipParseError::new(format!("Invalid integer at position {start}")))
    }

    /// Read the rest of the line until CRLF, unfolding continuation lines
    /// (CRLF + WSP) to a single SP. Consumes the final CRLF. Accepts bare LF.
    pub fn read_header_value(&mut self) -> String {
        let mut result = String::new();
        let start = self.pos;
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            if b == CR {
                result.push_str(&decode(&self.buf[start..self.pos]));
                if self.pos + 1 < self.buf.len() && self.buf[self.pos + 1] == LF {
                    // Continuation line?
                    if self.pos + 2 < self.buf.len() && is_wsp(self.buf[self.pos + 2]) {
                        result.push(' ');
                        self.pos += 3; // skip CR LF WSP
                        result.push_str(&self.read_header_value());
                        return result;
                    }
                    self.pos += 2; // consume CRLF
                    return result;
                }
            }
            if b == LF {
                // Bare LF (lenient).
                result.push_str(&decode(&self.buf[start..self.pos]));
                if self.pos + 1 < self.buf.len() && is_wsp(self.buf[self.pos + 1]) {
                    result.push(' ');
                    self.pos += 2;
                    result.push_str(&self.read_header_value());
                    return result;
                }
                self.pos += 1; // consume LF
                return result;
            }
            self.pos += 1;
        }
        // EOF without CRLF — return what we have.
        result.push_str(&decode(&self.buf[start..self.pos]));
        result
    }

    /// Expect and consume CRLF (also accepts bare LF). Errors otherwise.
    pub fn expect_crlf(&mut self) -> Result<(), SipParseError> {
        if self.pos < self.buf.len() && self.buf[self.pos] == CR {
            self.pos += 1;
        }
        if self.pos < self.buf.len() && self.buf[self.pos] == LF {
            self.pos += 1;
            return Ok(());
        }
        Err(SipParseError::new(format!("Expected CRLF at position {}", self.pos)))
    }

    /// Expect and consume a specific byte.
    pub fn expect(&mut self, byte: u8) -> Result<(), SipParseError> {
        if self.pos >= self.buf.len() || self.buf[self.pos] != byte {
            let got = self.buf.get(self.pos).copied().map(|b| b as i32).unwrap_or(-1);
            return Err(SipParseError::new(format!(
                "Expected byte 0x{byte:x} at position {}, got 0x{:x}",
                self.pos, got
            )));
        }
        self.pos += 1;
        Ok(())
    }

    /// At a CRLF (or bare LF) boundary?
    pub fn at_crlf(&self) -> bool {
        if self.pos >= self.buf.len() {
            return false;
        }
        if self.buf[self.pos] == LF {
            return true;
        }
        self.buf[self.pos] == CR && self.pos + 1 < self.buf.len() && self.buf[self.pos + 1] == LF
    }

    /// At the blank line marking end of headers? (Each header's trailing CRLF
    /// is already consumed by `read_header_value`, so this is a single CRLF /
    /// bare LF — or EOF.)
    pub fn at_end_of_headers(&self) -> bool {
        let p = self.pos;
        let len = self.buf.len();
        if p >= len {
            return true; // EOF counts as end of headers
        }
        if p + 1 < len && self.buf[p] == CR && self.buf[p + 1] == LF {
            return true;
        }
        self.buf[p] == LF
    }

    /// Consume the blank line (single CRLF or bare LF).
    pub fn consume_end_of_headers(&mut self) {
        if self.pos < self.buf.len() && self.buf[self.pos] == CR {
            self.pos += 1;
        }
        if self.pos < self.buf.len() && self.buf[self.pos] == LF {
            self.pos += 1;
        }
    }

    /// Remaining bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Slice the underlying buffer.
    pub fn slice(&self, start: usize, end: usize) -> &'a [u8] {
        &self.buf[start..end]
    }
}

/// Paranoid `1*DIGIT` decoder for SIP numeric header values. Accepts ONLY
/// bytes 0x30..=0x39 with at least one present. Rejects every `parseInt`-
/// tolerated injection shape (whitespace, sign, decimal point, `1e10`,
/// `0x10`, `Infinity`, `NaN`, empty). Overflow against `max` is detected
/// mid-loop so adversarially long inputs cannot wrap. `None` on any rejection.
pub fn strict_non_negative_decimal(s: &str, max: u64) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in bytes {
        if !(0x30..=0x39).contains(&c) {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((c - 0x30) as u64)?;
        if n > max {
            return None;
        }
    }
    Some(n)
}
