//! Whether caller text fits the RFC 3261 §25.1 class of the place a writer
//! puts it. A header line ends with CRLF and nowhere else (ADR-0053), and a
//! control byte other than HTAB stands in a value only as a quoted-pair's
//! escaped byte: text breaking either is refused, never written. Caller text
//! carries no NUL at all, so no peer reading a value up to its first NUL
//! reads another value than the one written. A header line is also held to
//! what the next hop's ingress reads ([`header_line`]). A caller holding text from
//! outside the stack checks it here and skips what is refused; a draft
//! refuses to freeze with a line it could not write whole, whichever writer
//! carried it ([`crate::draft::IncompleteDraft::Unwritable`]).

use std::fmt;

use crate::char_class::is_token_char;
use crate::control_bytes::{bare_control_byte, is_control, Escapes};
use crate::header::HeaderName;
use crate::parser::custom::headers::line_refusal;
use crate::parser::{FirstIngressBound, SipParserLimits};

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
    value_refusal(value, Escapes::of(None))
        .map_or(Ok(()), |reason| Err(refusal(Position::HeaderValue, reason)))
}

/// One header line: [`header_name`], its value holding a quoted-pair only in
/// a quoted-string or comment its header's grammar has, and nothing the
/// ingress of a next hop refuses: a line past the datagram and a value a gate
/// of its known header refuses (ADR-0001).
pub fn header_line(name: &str, value: &str) -> Result<(), WriteRefusal> {
    line_within(name, value, SipParserLimits::DEFAULT.max_header_length)
}

/// [`header_line`] on a new request (one with no To-tag), which its next hop
/// reads at first ingress: the line held to `bound` too.
pub fn new_request_line(
    name: &str,
    value: &str,
    bound: &FirstIngressBound,
) -> Result<(), WriteRefusal> {
    line_within(name, value, bound.header_length)
}

fn line_within(name: &str, value: &str, max_length: usize) -> Result<(), WriteRefusal> {
    header_name(name)?;
    let reason = value_refusal(value, Escapes::of(HeaderName::known(name).as_ref()))
        .or_else(|| Some(line_refusal(name, value, max_length)?.reason));
    match reason {
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
fn value_refusal(value: &str, escapes: Escapes) -> Option<String> {
    let at = value
        .bytes()
        .position(|b| b == 0)
        .or_else(|| bare_control_byte(value.as_bytes(), escapes))?;
    Some(byte_reason(value, at))
}

/// Why a rendered header value cannot be written: a control byte outside a
/// quoted-pair, CR and LF included. The check a freeze runs on every header
/// line it renders, relayed ones too: a quoted-pair a peer sent passes.
pub(crate) fn rendered_value(text: &[u8], escapes: Escapes) -> Option<String> {
    let at = bare_control_byte(text, escapes)?;
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
    fn a_quoted_pair_stands_only_where_the_header_quotes() {
        assert!(header_line("X-Note", "\"a \\\x07 b\"").is_ok());
        assert!(header_line("Organization", "\"a \\\x07 b\"").is_err());
        assert!(header_line("Server", "\"a \\\x07 b\"").is_err());
    }

    /// A value the next hop's ingress refuses is no line a writer sends.
    #[test]
    fn a_line_ingress_refuses_is_refused() {
        assert!(header_line("Expires", "3600").is_ok());
        for (name, value) in [
            ("Expires", "soon"),
            ("Min-SE", "ninety"),
            ("Content-Disposition", ""),
            ("Event", "a b"),
            ("Warning", "399 host \"open"),
        ] {
            assert!(header_line(name, value).is_err(), "{name}: {value}");
        }
        let bound = FirstIngressBound::DEFAULT;
        let long = "x".repeat(bound.header_length - "X-Long: ".len());
        let past = format!("{long}x");
        assert!(new_request_line("X-Long", &long, &bound).is_ok(), "at the first-ingress bound");
        assert!(new_request_line("X-Long", &past, &bound).is_err(), "one past it");
        assert!(header_line("X-Long", &past).is_ok(), "past first ingress, the datagram bounds");
        let tight = FirstIngressBound { header_length: 64, ..bound };
        assert!(new_request_line("X-Long", &"x".repeat(60), &tight).is_err(), "the bound stated");
        assert!(header_line("X-Long", &"x".repeat(65_535)).is_err(), "past the datagram");
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
            assert!(rendered_value(bad, Escapes::of(None)).is_some(), "{bad:?}");
            assert!(rendered_start_line(bad).is_some(), "{bad:?}");
        }
        assert!(
            rendered_value(b"\"a\\\0b\"", Escapes::of(None)).is_none(),
            "a quoted-pair a peer sent"
        );
        assert!(rendered_start_line(b"SIP/2.0 200 O\tK").is_none());
    }
}
