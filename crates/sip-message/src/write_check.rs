//! Whether caller text fits the RFC 3261 §25.1 class of the place a writer
//! puts it. A header line ends with CRLF and nowhere else (ADR-0053), and a
//! control byte other than HTAB stands in a value only as a quoted-pair's
//! escaped byte: text breaking either is refused, never written. Caller text
//! carries no NUL at all, so no peer reading a value up to its first NUL
//! reads another value than the one written. A caller holding text from
//! outside the stack checks it here and skips what is refused; a draft
//! refuses to freeze with a line it could not write whole, whichever writer
//! carried it ([`crate::draft::IncompleteDraft::Unwritable`]).

use std::fmt;

use crate::char_class::is_token_char;
use crate::control_bytes::{bare_control_byte, has_comments, is_control};
use crate::header::HeaderName;

/// Where a writer was asked to put text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// A header field name: a `token`.
    HeaderName,
    /// A header field value: no NUL, and no other control byte but HTAB
    /// outside a quoted-pair.
    HeaderValue,
    /// A status line's reason phrase: no control byte but HTAB.
    ReasonPhrase,
    /// Text a writer carries as a `quoted-string` (a display name, a
    /// parameter value that is no token): no CR, LF or NUL; the writer
    /// escapes every other control byte as a quoted-pair.
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

/// A header value: no NUL, and no other control byte but HTAB outside a
/// quoted-pair of a quoted-string.
pub fn header_value(value: &str) -> Result<(), WriteRefusal> {
    value_refusal(value, false).map_or(Ok(()), |reason| Err(refusal(Position::HeaderValue, reason)))
}

/// One header line: [`header_name`] and [`header_value`], a comment of a
/// header whose grammar has one carrying quoted-pairs too.
pub fn header_line(name: &str, value: &str) -> Result<(), WriteRefusal> {
    header_name(name)?;
    let comments = HeaderName::known(name).is_some_and(|known| has_comments(&known));
    match value_refusal(value, comments) {
        Some(reason) => Err(refusal(Position::HeaderValue, format!("{name}: {reason}"))),
        None => Ok(()),
    }
}

/// A reason phrase: any text but a control byte other than HTAB.
pub fn reason_phrase(text: &str) -> Result<(), WriteRefusal> {
    match text.bytes().position(is_control) {
        Some(at) => Err(refusal(Position::ReasonPhrase, byte_reason(text, at))),
        None => Ok(()),
    }
}

/// Text a writer quotes where it is no token (a display name, a `gen-value`):
/// any text but CR, LF and NUL, the rest escaped as the grammar asks.
pub fn quoted_text(text: &str) -> Result<(), WriteRefusal> {
    match text.bytes().position(|b| matches!(b, b'\r' | b'\n' | 0)) {
        Some(at) => Err(refusal(Position::QuotedText, byte_reason(text, at))),
        None => Ok(()),
    }
}

/// Why caller text cannot be a header value: a NUL, or a control byte outside
/// a quoted-pair.
fn value_refusal(value: &str, comments: bool) -> Option<String> {
    let at = value
        .bytes()
        .position(|b| b == 0)
        .or_else(|| bare_control_byte(value.as_bytes(), comments))?;
    Some(byte_reason(value, at))
}

/// Why a rendered header value cannot be written: a control byte outside a
/// quoted-pair, CR and LF included. The check a freeze runs on every header
/// line it renders, relayed ones too: a quoted-pair a peer sent passes.
pub(crate) fn rendered_value(text: &[u8], comments: bool) -> Option<String> {
    let at = bare_control_byte(text, comments)?;
    Some(byte_reason(&String::from_utf8_lossy(text), at))
}

/// Why a rendered start line cannot be written: a control byte other than
/// HTAB, CR and LF included, anywhere in it.
pub(crate) fn rendered_start_line(text: &[u8]) -> Option<String> {
    let at = text.iter().position(|&b| is_control(b))?;
    Some(byte_reason(&String::from_utf8_lossy(text), at))
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
        for bad in ["a\rb", "a\nb", "a\r\nVia: x", "a\0b", "\"a\\\r\""] {
            assert!(header_value(bad).is_err(), "{bad:?}");
            assert!(reason_phrase(bad).is_err(), "{bad:?}");
            assert!(quoted_text(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_comment_carries_a_quoted_pair_where_the_header_has_comments() {
        assert!(header_line("Server", "stack/1 (a \\\x07 b)").is_ok());
        assert!(header_line("X-Note", "stack/1 (a \\\x07 b)").is_err());
    }

    #[test]
    fn a_line_refusal_names_the_header() {
        let e = header_line("X-A", "a\r\nb").expect_err("refused");
        assert_eq!(e.position, Position::HeaderValue);
        assert!(e.reason.starts_with("X-A: byte 0x0d at 1"), "{e}");
    }

    #[test]
    fn a_freeze_finds_every_bare_control_byte() {
        for bad in [&b"a\0b"[..], b"a\rb", b"a\nb", b"a\x07b", b"\"a\\\r\""] {
            assert!(rendered_value(bad, false).is_some(), "{bad:?}");
            assert!(rendered_start_line(bad).is_some(), "{bad:?}");
        }
        assert!(rendered_value(b"\"a\\\0b\"", false).is_none(), "a quoted-pair a peer sent");
        assert!(rendered_start_line(b"SIP/2.0 200 O\tK").is_none());
    }
}
