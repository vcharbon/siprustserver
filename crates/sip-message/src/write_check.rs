//! Whether caller text fits the RFC 3261 §25.1 class of the place a writer
//! puts it. A header line ends with CRLF and nowhere else (ADR-0053): text
//! that would put a CR or LF inside one is refused, never written. A caller
//! holding text from outside the stack checks it here and skips what is
//! refused; a draft refuses to freeze with a line break inside a line
//! whichever writer carried it ([`crate::draft::IncompleteDraft::Unwritable`]).

use std::fmt;

use crate::char_class::is_token_char;

/// Where a writer was asked to put text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// A header field name: a `token`.
    HeaderName,
    /// A header field value: no CR, LF or NUL.
    HeaderValue,
    /// A status line's reason phrase: no CR, LF or NUL.
    ReasonPhrase,
    /// Text a writer carries as a `quoted-string` (a display name, a
    /// parameter value that is no token): no CR, LF or NUL.
    QuotedText,
    /// A request or status line as rendered.
    StartLine,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Position::HeaderName => "header name",
            Position::HeaderValue => "header value",
            Position::ReasonPhrase => "reason phrase",
            Position::QuotedText => "quoted text",
            Position::StartLine => "start line",
        })
    }
}

/// Text a writer refused: where it was to go and the first byte that does
/// not fit there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRefusal {
    pub position: Position,
    pub reason: String,
}

impl fmt::Display for WriteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} refused: {}", self.position, self.reason)
    }
}

impl std::error::Error for WriteRefusal {}

/// A header name: `token = 1*(alphanum / "-" / "." / "!" / "%" / "*" / "_" /
/// "+" / "`" / "'" / "~")`.
pub fn header_name(name: &str) -> Result<(), WriteRefusal> {
    if name.is_empty() {
        return Err(refusal(Position::HeaderName, "empty".to_string()));
    }
    match name.bytes().position(|b| !is_token_char(b)) {
        Some(at) => Err(refusal(Position::HeaderName, byte_reason(name, at))),
        None => Ok(()),
    }
}

/// A header value: any text but CR, LF and NUL.
pub fn header_value(value: &str) -> Result<(), WriteRefusal> {
    unbreakable(Position::HeaderValue, value)
}

/// One header line: [`header_name`] and [`header_value`].
pub fn header_line(name: &str, value: &str) -> Result<(), WriteRefusal> {
    header_name(name)?;
    header_value(value).map_err(|e| WriteRefusal { reason: format!("{name}: {}", e.reason), ..e })
}

/// A reason phrase: any text but CR, LF and NUL.
pub fn reason_phrase(text: &str) -> Result<(), WriteRefusal> {
    unbreakable(Position::ReasonPhrase, text)
}

/// Text a writer quotes where it is no token (a display name, a `gen-value`):
/// any text but CR, LF and NUL, the rest escaped as the grammar asks.
pub fn quoted_text(text: &str) -> Result<(), WriteRefusal> {
    unbreakable(Position::QuotedText, text)
}

/// The first CR or LF of `text` as a reason; `None` when it holds neither.
/// The one check a freeze runs on every line it renders.
pub(crate) fn line_break(text: &[u8]) -> Option<String> {
    let at = text.iter().position(|&b| b == b'\r' || b == b'\n')?;
    Some(byte_reason(&String::from_utf8_lossy(text), at))
}

fn unbreakable(position: Position, text: &str) -> Result<(), WriteRefusal> {
    match text.bytes().position(|b| matches!(b, b'\r' | b'\n' | 0)) {
        Some(at) => Err(refusal(position, byte_reason(text, at))),
        None => Ok(()),
    }
}

fn refusal(position: Position, reason: String) -> WriteRefusal {
    WriteRefusal { position, reason }
}

fn byte_reason(text: &str, at: usize) -> String {
    let byte = text.as_bytes().get(at).copied().unwrap_or_default();
    format!("byte 0x{byte:02x} at {at} in {text:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_name_is_a_token() {
        assert!(header_name("X-Custom_1.a!%*+`'~").is_ok());
        for bad in ["", "X/Bad", "X A", "X:A", "X-A\r\nVia", "Ü"] {
            assert!(header_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_value_holds_no_cr_lf_or_nul() {
        assert!(header_value("any text, \"quoted\", ü\t").is_ok());
        for bad in ["a\rb", "a\nb", "a\r\nVia: x", "a\0b"] {
            assert!(header_value(bad).is_err(), "{bad:?}");
            assert!(reason_phrase(bad).is_err(), "{bad:?}");
            assert!(quoted_text(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_line_refusal_names_the_header() {
        let e = header_line("X-A", "a\r\nb").expect_err("refused");
        assert_eq!(e.position, Position::HeaderValue);
        assert!(e.reason.starts_with("X-A: byte 0x0d at 1"), "{e}");
    }

    #[test]
    fn a_freeze_finds_cr_and_lf_but_lets_nul_through() {
        assert!(line_break(b"a\0b").is_none());
        assert!(line_break(b"a\rb").is_some());
        assert!(line_break(b"a\nb").is_some());
    }
}
