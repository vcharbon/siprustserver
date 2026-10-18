//! Byte-level cursor scanner. Port of `src/sip/parsers/custom/scanner.ts`.
//! All character classification is direct byte comparison — zero regex.
//!
//! The cursor never leaves its buffer: every read is checked access and every
//! move goes through `Scanner::bump`, which stops at the end, so the offset
//! arithmetic of the parser lives here and nowhere else (ADR-0051 §2). The
//! fallible methods (`expect`, `expect_crlf`) return `Result<_, SipParseError>`;
//! the rest are infallible.
#![deny(clippy::arithmetic_side_effects)]

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
/// (invalid sequences → U+FFFD). The common SIP path is pure ASCII: try the
/// `str::from_utf8` fast path first (word-at-a-time validation, one alloc) and
/// only fall back to the `Utf8Chunks`-based lossy machinery on genuinely
/// invalid input — output is byte-identical either way.
pub(crate) fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A byte range of the message image. Every token the scanner reads is one of
/// these — the parser slices the image by index and never copies a token.
pub type Span = std::ops::Range<usize>;

/// The text `span` covers in `image`, or the parse error a span outside it (or
/// splitting a character) is: spans come from a scanner over the same image,
/// so the error names a scanner defect, never accepted input.
pub fn span_text<'a>(image: &'a str, span: &Span) -> Result<&'a str, SipParseError> {
    image
        .get(span.clone())
        .ok_or_else(|| SipParseError::new(format!("span {span:?} outside the message image")))
}

/// Where the header block ends and how many lines it holds.
pub struct HeaderBlock {
    /// Byte offset just past the blank line, in raw-datagram coordinates.
    pub end: usize,
    /// Line count, including the start line — the header list's capacity.
    pub lines: usize,
}

/// Locate the blank line that ends the header block — the message's
/// header/body split. `end` is `raw.len()` when the datagram ends without a
/// blank line (the lenient EOF case [`Scanner::at_end_of_headers`] accepts).
///
/// Splitting first is what lets the body stay raw bytes while the header block
/// alone becomes the decoded text image: a binary body can never be mangled by
/// the decode, and the body is a slice of the original packet, not a copy. The
/// line count rides along so the header list can be sized without a second scan.
pub fn header_block(raw: &[u8]) -> HeaderBlock {
    let mut s = Scanner::new(raw);
    let mut lines: usize = 0;
    loop {
        if s.at_end() {
            return HeaderBlock { end: raw.len(), lines };
        }
        let line = s.take_while(|b| b != CR && b != LF);
        s.skip_if(CR);
        s.skip_if(LF);
        if line.is_empty() {
            return HeaderBlock { end: s.pos(), lines };
        }
        lines = lines.saturating_add(1);
    }
}

/// What one header line's value occupies in the image: the common case is a
/// plain [`Span`]; a line folded across CRLF+WSP has to be rebuilt (the fold
/// collapses to a single SP), so it comes back owned.
pub enum HeaderValue {
    Span(Span),
    Unfolded(String),
}

/// `range` with leading/trailing whitespace excluded. Trimming a span is index
/// arithmetic — no copy, unlike trimming an owned value. `None` when `range`
/// is not a span of `image`.
pub fn trim_span(image: &str, range: Span) -> Option<Span> {
    let slice = image.get(range.clone())?;
    let after_lead = slice.trim_start();
    let trimmed = after_lead.trim_end();
    if trimmed.is_empty() {
        return Some(range.start..range.start);
    }
    let start = range.start.checked_add(slice.len().checked_sub(after_lead.len())?)?;
    Some(start..start.checked_add(trimmed.len())?)
}

/// A cursor over one buffer. Its position never passes the buffer's end.
pub struct Scanner<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// The cursor's offset into the buffer.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Whether every byte has been read.
    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Moves the cursor `n` bytes on, stopping at the end of the buffer.
    fn bump(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.buf.len());
    }

    /// Peek at the current byte without advancing. `None` at end.
    pub fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// The byte `offset` past the current one, without advancing. `None` past
    /// the end.
    pub fn peek_at(&self, offset: usize) -> Option<u8> {
        self.buf.get(self.pos.checked_add(offset)?).copied()
    }

    /// Read the current byte and advance. `None` at end.
    pub fn advance(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.bump(1);
        }
        b
    }

    /// Consumes the current byte when it is `byte`; whether it did.
    pub fn skip_if(&mut self, byte: u8) -> bool {
        let hit = self.peek() == Some(byte);
        if hit {
            self.bump(1);
        }
        hit
    }

    /// Consumes bytes while `keep` holds for them; the span read.
    pub fn take_while(&mut self, mut keep: impl FnMut(u8) -> bool) -> Span {
        let start = self.pos;
        while self.peek().is_some_and(&mut keep) {
            self.bump(1);
        }
        start..self.pos
    }

    /// Consumes bytes up to `byte`, which is left unread; the span read.
    pub fn take_until(&mut self, byte: u8) -> Span {
        self.take_while(|b| b != byte)
    }

    /// Skip over SP and HTAB.
    pub fn skip_wsp(&mut self) {
        self.take_while(is_wsp);
    }

    /// Skip LWS: SP/HTAB and header-folding (CRLF followed by SP/HTAB).
    pub fn skip_lws(&mut self) {
        loop {
            match self.peek() {
                Some(b) if is_wsp(b) => self.bump(1),
                Some(CR) if self.peek_at(1) == Some(LF) && self.peek_at(2).is_some_and(is_wsp) => {
                    self.bump(3); // CR LF WSP
                }
                _ => break,
            }
        }
    }

    /// The line ending at the cursor, as its length: CRLF, or a bare LF
    /// (lenient). `None` elsewhere.
    fn eol_len(&self) -> Option<usize> {
        match self.peek()? {
            CR if self.peek_at(1) == Some(LF) => Some(2),
            LF => Some(1),
            _ => None,
        }
    }

    /// Read the rest of the line until CRLF, unfolding continuation lines
    /// (CRLF + WSP) to a single SP. Consumes the final CRLF. Accepts bare LF.
    /// An unfolded line is the only shape that cannot be expressed as a span.
    /// One pass, constant stack: each line is copied once into the unfolded
    /// value, whatever the number of continuation lines.
    pub fn read_header_value(&mut self) -> Result<HeaderValue, SipParseError> {
        let mut line = self.pos;
        let mut unfolded: Option<String> = None;
        while !self.at_end() {
            let Some(eol) = self.eol_len() else {
                self.bump(1);
                continue;
            };
            let end = self.pos;
            self.bump(eol);
            if !self.peek().is_some_and(is_wsp) {
                return self.finish_value(line..end, unfolded);
            }
            self.bump(1); // the fold's WSP: the fold becomes one SP
            let text = self.bytes(line..end)?;
            let out = unfolded.get_or_insert_with(String::new);
            out.push_str(&String::from_utf8_lossy(text));
            out.push(' ');
            line = self.pos;
        }
        // EOF without CRLF — the line runs to the end of the image.
        self.finish_value(line..self.pos, unfolded)
    }

    /// The bytes of a span this scanner read.
    fn bytes(&self, span: Span) -> Result<&'a [u8], SipParseError> {
        self.buf
            .get(span.clone())
            .ok_or_else(|| SipParseError::new(format!("span {span:?} outside the scanned bytes")))
    }

    /// The value whose last line is `last`: that span itself when nothing was
    /// folded, else the unfolded text it completes.
    fn finish_value(
        &self,
        last: Span,
        unfolded: Option<String>,
    ) -> Result<HeaderValue, SipParseError> {
        Ok(match unfolded {
            None => HeaderValue::Span(last),
            Some(mut out) => {
                out.push_str(&String::from_utf8_lossy(self.bytes(last)?));
                HeaderValue::Unfolded(out)
            }
        })
    }

    /// Expect and consume CRLF (also accepts bare LF). Errors otherwise.
    pub fn expect_crlf(&mut self) -> Result<(), SipParseError> {
        self.skip_if(CR);
        if self.skip_if(LF) {
            return Ok(());
        }
        Err(SipParseError::new(format!("Expected CRLF at position {}", self.pos)))
    }

    /// Expect and consume a specific byte.
    pub fn expect(&mut self, byte: u8) -> Result<(), SipParseError> {
        if self.skip_if(byte) {
            return Ok(());
        }
        let got = self.peek().map_or(-1, i32::from);
        Err(SipParseError::new(format!(
            "Expected byte 0x{byte:x} at position {}, got 0x{got:x}",
            self.pos
        )))
    }

    /// At the blank line marking end of headers? (Each header's trailing CRLF
    /// is already consumed by `read_header_value`, so this is a single CRLF /
    /// bare LF — or EOF.)
    pub fn at_end_of_headers(&self) -> bool {
        self.at_end() || self.eol_len().is_some()
    }

    /// Consume the blank line (single CRLF or bare LF).
    pub fn consume_end_of_headers(&mut self) {
        self.skip_if(CR);
        self.skip_if(LF);
    }
}

/// Paranoid `1*DIGIT` decoder for SIP numeric header values. Accepts ONLY
/// bytes 0x30..=0x39 with at least one present. Rejects every `parseInt`-
/// tolerated injection shape (whitespace, sign, decimal point, `1e10`,
/// `0x10`, `Infinity`, `NaN`, empty). Overflow against `max` is detected
/// mid-loop so adversarially long inputs cannot wrap. `None` on any rejection.
pub fn strict_non_negative_decimal(s: &str, max: u64) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(char::from(c).to_digit(10)?))?;
        if n > max {
            return None;
        }
    }
    Some(n)
}
