//! The one reader of an address value, in both readings (ADR-0053 §4, §6).

use std::ops::Range;

use crate::char_class::is_token_char;
use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::super::params::{read_params, ParamReading, Params};
use super::super::scan::{
    byte_at, clamped, index_of, quoted_close, read_quoted, skip_ws, sub, sub_trimmed,
};
use super::super::uri::{Uri, UriRole};
use super::{AddrForm, BareAddrSpec, NameAddr};

/// The grammar of one header's address value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddrGrammar {
    pub(crate) form: AddrForm,
    pub(crate) role: UriRole,
    /// Header parameters follow the address. Without them (RFC 3325 §9.1) a
    /// bare value is one URI whole, and nothing follows a `>`.
    pub(crate) header_params: bool,
    /// A bare value whose `;` comes before an `@` outside any quoted-string
    /// reads its URI up to that `@` and the next `;`, once the strict read
    /// failed: a shape the ABNF admits and RFC 3261 §20 brackets. The ingress
    /// gate refuses it unless a deployment names it (ADR-0001, exemptions).
    pub(crate) bare_user_semicolon: bool,
    /// What a bare addr-spec may hold beside header parameters.
    pub(crate) bare: BareAddrSpec,
}

impl AddrGrammar {
    /// `form` in `role`, with header parameters, a bare value held to §20.
    pub(crate) const fn of(form: AddrForm, role: UriRole) -> Self {
        Self {
            form,
            role,
            header_params: true,
            bare_user_semicolon: false,
            bare: BareAddrSpec::Bracketed,
        }
    }
}

/// One address value read, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddrRead {
    pub(crate) addr: NameAddr,
    /// The byte range of the raw value the URI's text takes.
    pub(crate) uri_span: Range<usize>,
    /// The URI was read by [`AddrGrammar::bare_user_semicolon`].
    pub(crate) bare_user_semicolon: bool,
}

/// The display name and the position of the `<` that follows it, `None` for a
/// bare value; `Err` for an open quote, which only the lenient reading reads,
/// to the end.
fn display_part(value: &SipStr, strict: bool) -> Result<(Option<SipStr>, Option<usize>), String> {
    let bytes = value.as_bytes();
    if byte_at(bytes, 0) == Some(b'"') {
        let (text, lenient_end) = read_quoted(value, 0);
        let after = match quoted_close(bytes, 0) {
            Some(after) => after,
            None if strict => {
                return Err(format!("unterminated quoted display name: {:?}", value.as_str()))
            }
            None => lenient_end,
        };
        let open = skip_ws(bytes, after);
        return Ok((Some(text), Some(open)));
    }
    let run = display_run_end(bytes);
    let display = sub_trimmed(value, 0, run);
    let open = (byte_at(bytes, run) == Some(b'<')).then_some(run);
    Ok((open.filter(|_| !display.is_empty()).map(|_| display), open))
}

pub(super) fn read_strict(raw: &SipStr, grammar: &AddrGrammar) -> Result<AddrRead, SipParseError> {
    let lead = raw.as_str().len() - crate::whitespace::trim_start(raw.as_str()).len();
    let value = raw.trimmed();
    let bytes = value.as_bytes();
    let (display, open) = display_part(&value, true).map_err(SipParseError::new)?;
    let (uri_span, params_at) = match open {
        Some(open) => angle_uri(&value, open)?,
        None => bare_uri_span(&value, grammar)?,
    };
    let uri = Uri::read_exact(&sub(&value, uri_span.start, uri_span.end), grammar.role)?;
    let params = if grammar.header_params {
        read_params(&value, params_at, ParamReading::Strict, None)?.0
    } else if skip_ws(bytes, params_at) < bytes.len() {
        return Err(SipParseError::new(format!(
            "header parameters where the grammar has none: {:?}",
            value.as_str()
        )));
    } else {
        Params::new()
    };
    let span = lead + uri_span.start..lead + uri_span.end;
    Ok(AddrRead {
        addr: NameAddr { display, uri, params },
        uri_span: span,
        bare_user_semicolon: false,
    })
}

/// The URI of a bare value and where its header parameters start: up to the
/// first `;` with header parameters, the whole value without.
fn bare_uri_span(
    value: &SipStr,
    grammar: &AddrGrammar,
) -> Result<(Range<usize>, usize), SipParseError> {
    let bytes = value.as_bytes();
    let semi = if grammar.header_params {
        index_of(bytes, b';', 0).unwrap_or(bytes.len())
    } else {
        bytes.len()
    };
    // No URI byte is a `<`: one before the first `;` opens a name-addr whose
    // display name is neither tokens nor quoted.
    if index_of(bytes, b'<', 0).is_some_and(|lt| lt < semi) {
        return Err(SipParseError::new(format!(
            "display name is neither tokens nor a quoted-string: {:?}",
            value.as_str()
        )));
    }
    if grammar.form == AddrForm::NameAddrOnly {
        return Err(SipParseError::new(format!(
            "a bare addr-spec where the grammar requires <uri>: {:?}",
            value.as_str()
        )));
    }
    let spec = clamped(value.as_str(), 0, semi);
    let bracketed: &[char] = match grammar.bare {
        BareAddrSpec::Bracketed => &[',', '?'],
        BareAddrSpec::UriHeaders => &[','],
        BareAddrSpec::UriHeadersAndComma => &[],
    };
    if grammar.header_params && spec.contains(bracketed) {
        return Err(SipParseError::new(format!(
            "bare addr-spec with `,` or `?`; name-addr \"<uri>\" form required (RFC 3261 §20): {:?}",
            crate::whitespace::trim_end(spec)
        )));
    }
    Ok((0..crate::whitespace::trim_end(spec).len(), semi))
}

/// The [`AddrGrammar::bare_user_semicolon`] reading of a bare value, `None`
/// when it does not read that way either.
pub(super) fn read_bare_user_semicolon(raw: &SipStr, grammar: &AddrGrammar) -> Option<AddrRead> {
    let lead = raw.as_str().len() - crate::whitespace::trim_start(raw.as_str()).len();
    let value = raw.trimmed();
    let bytes = value.as_bytes();
    if byte_at(bytes, 0) == Some(b'"') || index_of(bytes, b'<', 0).is_some() {
        return None;
    }
    let at = unquoted_at(bytes)?;
    if index_of(bytes, b';', 0).is_none_or(|semi| semi > at) {
        return None;
    }
    let end = index_of(bytes, b';', at).unwrap_or(bytes.len());
    let uri = Uri::read_exact(&sub(&value, 0, end), grammar.role).ok()?;
    let (params, _) = read_params(&value, end, ParamReading::Strict, None).ok()?;
    let addr = NameAddr { display: None, uri, params };
    Some(AddrRead { addr, uri_span: lead..lead + end, bare_user_semicolon: true })
}

/// The first `@` of `bytes` outside any quoted-string.
fn unquoted_at(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while let Some(b) = byte_at(bytes, i) {
        match b {
            b'@' => return Some(i),
            b'"' => i = quoted_close(bytes, i)?,
            _ => i += 1,
        }
    }
    None
}

pub(super) fn read_lenient(raw: &SipStr, grammar: &AddrGrammar) -> AddrRead {
    let lead = raw.as_str().len() - crate::whitespace::trim_start(raw.as_str()).len();
    let value = raw.trimmed();
    let bytes = value.as_bytes();
    let (display, open) = display_part(&value, false).unwrap_or((None, None));
    let (uri_span, params_at) = match open {
        Some(open) if byte_at(bytes, open) == Some(b'<') => match index_of(bytes, b'>', open + 1) {
            Some(close) => (open + 1..close, close + 1),
            None => (open + 1..bytes.len(), bytes.len()),
        },
        Some(open) => (open..bytes.len(), bytes.len()),
        None => {
            let semi = index_of(bytes, b';', 0).unwrap_or(bytes.len());
            (0..crate::whitespace::trim_end(clamped(value.as_str(), 0, semi)).len(), semi)
        }
    };
    let text = sub_trimmed(&value, uri_span.start, uri_span.end);
    let uri = Uri::read_exact(&text, grammar.role).unwrap_or_else(|_| Uri::opaque(text));
    let params = match read_params(&value, params_at, ParamReading::Lenient, None) {
        Ok((params, _)) => params,
        Err(_) => Params::new(),
    };
    let span = lead + uri_span.start..lead + uri_span.end;
    AddrRead { addr: NameAddr { display, uri, params }, uri_span: span, bare_user_semicolon: false }
}

/// The end of the leading run of token characters and LWS — the extent an
/// unquoted display name may take before its `<`.
fn display_run_end(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|&b| !(is_token_char(b) || b == b' ' || b == b'\t'))
        .unwrap_or(bytes.len())
}

/// The byte range of the URI inside `<...>` opening at byte `open`, untrimmed,
/// plus the position of the header parameters that follow it.
fn angle_uri(value: &SipStr, open: usize) -> Result<(Range<usize>, usize), SipParseError> {
    let bytes = value.as_bytes();
    if byte_at(bytes, open) != Some(b'<') {
        return Err(SipParseError::new(format!(
            "name-addr display name is not followed by <uri>: {:?}",
            value.as_str()
        )));
    }
    match index_of(bytes, b'>', open + 1) {
        Some(close) => {
            let inner = clamped(value.as_str(), open + 1, close);
            if inner != inner.trim_matches([' ', '\t']) {
                return Err(SipParseError::new(format!(
                    "LWS inside \"<...>\" addr-spec: {:?}",
                    value.as_str()
                )));
            }
            Ok((open + 1..close, close + 1))
        }
        None => {
            Err(SipParseError::new(format!("name-addr has no closing `>`: {:?}", value.as_str())))
        }
    }
}
