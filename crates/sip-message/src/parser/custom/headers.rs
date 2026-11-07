//! SIP header-block parser. No regex. Handles folding, compact-form
//! expansion, Content-Length extraction and case-preserving names. A value is
//! trimmed of SP and HTAB alone, then meets the grammar's control-byte rule
//! (one only as the escaped byte of a quoted-pair, in a quoted-string or
//! comment its header's grammar has) and the one-line rule of the
//! single-valued headers (`header::SINGLE_VALUED`, compact forms counted),
//! then the gates beyond the grammar (ADR-0001): the registry-driven strict
//! numeric values and the CVE-regression quoted-string / Digest checks.

use super::compact_forms::expand_compact_form;
pub use super::name_forms::scan_header_name_forms;
use super::scanner::{
    is_wsp, strict_non_negative_decimal, trim_span, HeaderValue, Scanner, Span, COLON, CR, HTAB, LF,
};
use super::vouched::{is_vouched_value, vouched_reading};
use crate::char_class::is_token_char;
use crate::control_bytes::{bare_control_byte, is_control, Escapes};
use crate::error::SipParseError;
use crate::header::{
    is_single_valued, quoted_close, read_params, Authorization, HeaderId, HeaderName,
    HeaderValue as _, ParamReading,
};
use crate::header_table::{HeaderTable, Ids};
use crate::parser::SipParserLimits;
use crate::sip_str::{SharedText, SipStr};
use crate::types::SipHeader;

const UINT_32_MAX: u64 = (1u64 << 32) - 1;
const INT_32_MAX: u64 = (1u64 << 31) - 1;

struct NumericHeaderRule {
    min: u64,
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
    let r = |min, max, tail| Some(NumericHeaderRule { min, max, tail });
    match name {
        HeaderName::ContentLength => r(0, INT_32_MAX, NumericTail::None),
        // §20.16: any 32-bit value; §8.1.1.5's bound is the sender's.
        HeaderName::CSeq => r(0, UINT_32_MAX, NumericTail::Method),
        HeaderName::MaxForwards => r(0, 255, NumericTail::None),
        HeaderName::Expires | HeaderName::MinExpires => r(0, UINT_32_MAX, NumericTail::None),
        HeaderName::SessionExpires | HeaderName::MinSe => r(0, UINT_32_MAX, NumericTail::Params),
        // RFC 3262 §7.1: `response-num`, 1 to 2^32-1.
        HeaderName::RSeq => r(1, UINT_32_MAX, NumericTail::None),
        _ => None,
    }
}

/// The number a trimmed numeric value states, `None` unless its digits are
/// followed by nothing but the tail its grammar has: the strict parameter
/// reading of a session timer's, the Method the CSeq reader reads.
fn strict_numeric_value(value: &SipStr, rule: &NumericHeaderRule) -> Option<u64> {
    let text = value.as_str();
    let digits = match rule.tail {
        NumericTail::None => text,
        // `SEMI = SWS ";" SWS`: whitespace may stand before the `;`.
        NumericTail::Params => {
            let at = text.find(';').unwrap_or(text.len());
            read_params(value, at, ParamReading::Strict, None).ok()?;
            crate::whitespace::trim_end(text.get(..at)?)
        }
        NumericTail::Method => text.split_once([' ', '\t']).map_or(text, |(head, _)| head),
    };
    strict_non_negative_decimal(digits, rule.max).filter(|&n| n >= rule.min)
}

/// The single-valued headers the split has met (`header::SINGLE_VALUED`), so a
/// second line of one, compact forms counted, is refused (RFC 3261 §7.3.1),
/// and the declared Content-Length.
#[derive(Default)]
struct Singletons {
    content_length: Option<u64>,
    /// The known single-valued headers met, as [`HeaderId::bit`]s.
    known: u128,
    /// Whether the one listed extension header was met.
    extension: bool,
}

impl Singletons {
    /// Notes a line of `name`; whether one was met before, `false` for a
    /// header that may repeat.
    fn repeated(&mut self, name: &HeaderName) -> bool {
        if !is_single_valued(name) {
            return false;
        }
        match name.key().id() {
            Some(id) => {
                let met = self.known & id.bit() != 0;
                self.known |= id.bit();
                met
            }
            None => std::mem::replace(&mut self.extension, true),
        }
    }
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

/// Whether a header block must end with its blank line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlankLine {
    /// A message's: its head ends with an empty line (RFC 3261 §7), and one
    /// that ends without it is refused, every line still read.
    Required,
    /// A message fragment's (RFC 3420 §2: `[ CRLF [ message-body ] ]`).
    Optional,
}

/// Parse all headers from the current position until the blank line, which is
/// consumed. `text` is the message image the scanner walks; every name/value
/// comes back as a span of it, so a header costs no allocation.
///
/// A line that breaks the grammar or a gate is left out and the split reads
/// on, the first such line kept as the refusal; a block that ends with no
/// blank line where `blank_line` requires one is refused after its last line.
/// A line past the length limit stops the split: the `Err` is that first
/// refused line, else the limit.
pub fn parse_headers(
    s: &mut Scanner,
    text: &SharedText,
    lines: usize,
    limits: &SipParserLimits,
    blank_line: BlankLine,
) -> Result<ParsedHeaders, SipParseError> {
    // Size the list up front from the block's line count — growing it from
    // empty cost one reallocation per doubling, several per message.
    let mut kept = Kept { lines: Vec::with_capacity(lines), ids: Ids::with_capacity(lines) };
    let mut refusal: Option<SipParseError> = None;
    let mut echoed_line_refused = false;
    let mut singletons = Singletons::default();

    loop {
        if s.at_end() {
            if blank_line == BlankLine::Required {
                refusal.get_or_insert_with(|| {
                    SipParseError::new("header block ends with no blank line (RFC 3261 §7)")
                });
            }
            break;
        }
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
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
    // A name that stops short of its colon did not read either: what was read
    // may be a part of any name, the five a response echoes included.
    if let Err(error) = s.expect(COLON) {
        return refused(s, error, true);
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
        HeaderValue::Unfolded(v) => SipStr::owned(crate::whitespace::trim(&v)),
    };

    let header = known.clone().unwrap_or_else(|| HeaderName::Other(name.clone()));
    let shown = crate::error::reason_text::header(&header);

    // Bound per-header memory: name + ": " + value, post-unfold/trim.
    let header_len = name.len() + 2 + trimmed_value.len();
    if header_len > limits.max_header_length {
        return Err(SipParseError::new(format!(
            "Header {shown} length {header_len} exceeds limit {}",
            limits.max_header_length
        )));
    }

    let echoed = known.as_ref().is_some_and(is_echoed);
    if s.escaped_fold() && known.as_ref().is_some_and(is_quoted_string_header) {
        let error = SipParseError::new(format!(
            "Header {shown} escapes a line fold with `\\` (RFC 3261 §25.1)"
        ));
        return Ok(Line::Refused { error, echoed });
    }
    let escapes = Escapes::of(known.as_ref());
    if let Some(at) = value_control_byte(trimmed_value.as_bytes(), known.as_ref(), escapes) {
        let byte = trimmed_value.as_bytes().get(at).copied().unwrap_or_default();
        let error = SipParseError::new(format!(
            "Header {shown} holds control byte 0x{byte:02x} outside a quoted-pair \
             (RFC 3261 §25.1)"
        ));
        return Ok(Line::Refused { error, echoed });
    }
    let counted = limits.value_gates || header == HeaderName::ContentLength;
    if counted && singletons.repeated(&header) {
        let error =
            SipParseError::new(format!("Multiple {shown} headers (RFC 3261 §7.3.1 — one value)"));
        return Ok(Line::Refused { error, echoed });
    }
    if let Some(known) = known {
        let gates = limits.value_gates;
        if let Some(error) = gate_refusal(&known, &trimmed_value, singletons, gates) {
            return Ok(Line::Refused { error, echoed });
        }
        if let Some(Err(e)) =
            vouched_reading(&known, &trimmed_value).filter(|_| limits.wire_grammar)
        {
            let error = SipParseError::new(format!("Malformed {shown} header: {}", e.reason));
            return Ok(Line::Refused { error, echoed });
        }
    }

    kept.lines.push(SipHeader { name, value: trimmed_value });
    kept.ids.push(id);
    Ok(Line::Kept)
}

/// The offset of the first control byte of `value` that is no quoted-pair's
/// escaped byte, a quoted-pair standing only in a quoted-string or comment of
/// the header's grammar (`Escapes::of`). A Replaces' call-id is `word`s, whose
/// `"` opens no quoted-string (RFC 3891 §6.1), so the scan for one starts
/// after it.
fn value_control_byte(value: &[u8], known: Option<&HeaderName>, escapes: Escapes) -> Option<usize> {
    let lead = match known {
        Some(HeaderName::Replaces) => value.iter().position(|&b| b == b';').unwrap_or(value.len()),
        _ => 0,
    };
    let (word, rest) = value.split_at_checked(lead)?;
    word.iter()
        .position(|&b| is_control(b))
        .or_else(|| bare_control_byte(rest, escapes).map(|at| at + lead))
}

/// The refusal the value of the known header `known` draws, if any: a gate
/// beyond the grammar (ADR-0001). A Content-Length that passes is recorded in
/// `singletons`. With `gates` off only Content-Length is judged, which frames
/// the message.
fn gate_refusal(
    known: &HeaderName,
    value: &SipStr,
    singletons: &mut Singletons,
    gates: bool,
) -> Option<SipParseError> {
    if !gates && *known != HeaderName::ContentLength {
        return None;
    }
    let name = crate::error::reason_text::header(known);

    // Registry-driven paranoid digit-only pass.
    if let Some(rule) = numeric_header_rule(known) {
        let Some(parsed) = strict_numeric_value(value, &rule) else {
            return Some(SipParseError::new(format!(
                "Invalid {name} numeric value: {:?}",
                value.as_str()
            )));
        };
        if *known == HeaderName::ContentLength {
            singletons.content_length = Some(parsed);
        }
    }

    // A header whose grammar needs a value and which ingress vouches for.
    if value.is_empty() && is_vouched_value(known) {
        return Some(SipParseError::new(format!("Empty {name} header (RFC 3261 §25.1)")));
    }

    // Quoted-strings held to the grammar on quoted-string-bearing headers
    // (CVE-2023-27599).
    if is_quoted_string_header(known) {
        if let Some(why) = quoted_string_refusal(value.as_bytes()) {
            return Some(SipParseError::new(format!("{why} quoted-string in {name} header")));
        }
    }

    // Credentials read whole by their one reader, whatever the scheme
    // (CVE-2023-28098, RFC 3261 §22.4).
    if is_authorization_header(known) {
        if let Err(e) = Authorization::parse(value) {
            return Some(SipParseError::new(format!(
                "Malformed credentials in {name} header: {}",
                e.reason
            )));
        }
    }
    None
}

/// Why the ingress of a next hop would refuse the line `name: value`, if it
/// would: longer than `max_length` (the line bound the message meets there),
/// or held by a value gate or the vouched reading of a known header
/// (ADR-0001). The check a writer runs on caller text (`write_check`); the
/// control-byte rule is the writer's own.
pub(crate) fn line_refusal(name: &str, value: &str, max_length: usize) -> Option<SipParseError> {
    let header_len = name.len().saturating_add(2).saturating_add(value.len());
    if header_len > max_length {
        let header = HeaderName::of(&SipStr::owned(name));
        let shown = crate::error::reason_text::header(&header);
        return Some(SipParseError::new(format!(
            "Header {shown} length {header_len} exceeds the bound {max_length}"
        )));
    }
    let known = HeaderName::known(name)?;
    let value = SipStr::owned(value);
    gate_refusal(&known, &value, &mut Singletons::default(), true)
        .or_else(|| vouched_reading(&known, &value)?.err())
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

/// Why a quoted-string of `value` breaks the grammar (RFC 3261 §25.1), if one
/// does: none is closed, or a byte inside it is neither `qdtext` nor a
/// `quoted-pair` of one ASCII byte.
fn quoted_string_refusal(value: &[u8]) -> Option<&'static str> {
    let mut i = 0;
    while let Some(&b) = value.get(i) {
        if b != b'"' {
            i += 1;
            continue;
        }
        match quoted_close(value, i) {
            Some(after) => i = after,
            None if value.get(i + 1..).is_some_and(|rest| rest.contains(&b'"')) => {
                return Some("Malformed")
            }
            None => return Some("Unterminated"),
        }
    }
    None
}

/// Read the header name until colon, rejecting only unambiguously-illegal
/// bytes (CTL except HTAB, DEL) — preserves the RFC 4475 wide-range torture
/// test while catching CVE-2023-27598. Trailing WSP before the colon is
/// permitted (RFC 4475 §3.1.1.1).
pub(super) fn read_header_name(s: &mut Scanner) -> Result<Span, SipParseError> {
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
