//! [`NameAddr`] — the `[display-name] <uri> *(";" param)` core every
//! address-valued header is built from.

use std::ops::Range;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::params::{ParamValue, Params};
use super::reading::Reading;

mod read;
use super::scan::{byte_at, index_of, read_quoted, skip_ws, sub_trimmed};
use super::uri::{Uri, UriRole};
use super::wire::Wire;
use read::{read_bare_user_semicolon, read_lenient, read_strict};
pub(crate) use read::{AddrGrammar, AddrRead};

/// An address with an optional display name and header parameters. The URI is
/// grammar-mandatory, so it is not optional here either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameAddr {
    display: Option<SipStr>,
    uri: Uri,
    params: Params,
}

impl NameAddr {
    pub fn new(uri: Uri) -> Self {
        Self { display: None, uri, params: Params::new() }
    }

    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// The display name, unescaped.
    pub fn display(&self) -> Option<&str> {
        self.display.as_ref().map(SipStr::as_str)
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn with_uri(mut self, uri: Uri) -> Self {
        self.uri = uri;
        self
    }

    pub fn with_display(mut self, display: impl Into<SipStr>) -> Self {
        self.display = Some(display.into());
        self
    }

    pub fn without_display(mut self) -> Self {
        self.display = None;
        self
    }

    pub fn with_param(mut self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        self.params.set(name, value);
        self
    }

    pub fn without_param(mut self, name: &str) -> Self {
        self.params.remove(name);
        self
    }

    /// The address alone — what a header whose grammar has no parameters can
    /// carry.
    pub fn without_params(mut self) -> Self {
        self.params = Params::new();
        self
    }

    /// Read `( name-addr / addr-spec ) *( SEMI generic-param )` — the
    /// [`AddrForm::NameAddrOrSpec`], [`UriRole::Target`] reading of
    /// [`parse_as`](Self::parse_as).
    pub fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        Self::parse_as(raw, AddrForm::NameAddrOrSpec, UriRole::Target)
    }

    /// Read a target stated outside a header line, bare or as a name-addr:
    /// text that reads whole as a URI is that URI, so a `;` in its user part
    /// stays there (RFC 3261 §25.1); any other text is read by
    /// [`parse`](Self::parse) (angle brackets, a display name, header
    /// parameters).
    pub fn parse_uri_or_name_addr(raw: &SipStr) -> Result<Self, SipParseError> {
        match Uri::parse(raw) {
            Ok(uri) => Ok(Self::new(uri)),
            Err(_) => Self::parse(raw),
        }
    }

    /// Read one address value whole by the RFC 3261 §25.1 grammar:
    /// `name-addr = [ display-name ] LAQUOT addr-spec RAQUOT`, a display name
    /// being a quoted-string or tokens separated by LWS, the addr-spec read by
    /// the one URI reader in `role` with no whitespace inside the brackets,
    /// then the header parameters and nothing else. A bare addr-spec ends at
    /// the first `;`, every parameter after it is the header's, and it carries
    /// no `,` or `?` ([`BareAddrSpec::Bracketed`], §20);
    /// [`AddrForm::NameAddrOnly`] refuses it.
    pub fn parse_as(raw: &SipStr, form: AddrForm, role: UriRole) -> Result<Self, SipParseError> {
        Self::read(raw, &AddrGrammar::of(form, role), Reading::Strict).map(|read| read.addr)
    }

    /// [`parse_as`](Self::parse_as), with the byte range of `raw` the URI's
    /// text takes: the seam a rewrite of the URI alone splices at, every other
    /// byte kept as written.
    pub(crate) fn parse_spanned(
        raw: &SipStr,
        form: AddrForm,
        role: UriRole,
    ) -> Result<(Self, Range<usize>), SipParseError> {
        Self::read(raw, &AddrGrammar::of(form, role), Reading::Strict)
            .map(|read| (read.addr, read.uri_span))
    }

    /// The one reader of an address value, in `grammar`. [`Reading::Strict`]
    /// is [`parse_as`](Self::parse_as), with the grammar's two variations: no
    /// header parameters, where a bare value is one URI whole; and the
    /// [`AddrGrammar::bare_user_semicolon`] reading, tried only when the
    /// strict read of a bare value fails. [`Reading::Lenient`] reads a value
    /// the strict reading takes the same way, and never refuses one it does
    /// not: an open quote or `<` runs to the end, the parameters read on past
    /// a byte off their grammar, and a URI the URI reader refuses is held
    /// [`Uri::opaque`].
    pub(crate) fn read(
        raw: &SipStr,
        grammar: &AddrGrammar,
        reading: Reading,
    ) -> Result<AddrRead, SipParseError> {
        let strict = match read_strict(raw, grammar) {
            Err(refusal) if grammar.bare_user_semicolon => {
                read_bare_user_semicolon(raw, grammar).ok_or(refusal)
            }
            read => read,
        };
        match reading {
            Reading::Strict => strict,
            Reading::Lenient => Ok(strict.unwrap_or_else(|_| read_lenient(raw, grammar))),
        }
    }

    /// Read an identity a caller states as text, its URI a [`UriRole::Identity`]:
    /// bracketed, the name-addr [`parse_as`] reads; bare, an addr-spec whose
    /// every `;`-parameter is a URI parameter, the way a Request-URI reads. The
    /// two readings differ only on a bare value's tail.
    ///
    /// [`parse_as`]: Self::parse_as
    pub fn parse_identity(raw: &SipStr) -> Result<Self, SipParseError> {
        if raw.as_str().contains('<') {
            Self::parse_as(raw, AddrForm::NameAddrOrSpec, UriRole::Identity)
        } else {
            Uri::parse_as(raw, UriRole::Identity).map(Self::new)
        }
    }

    /// Read an identity stated for a From or To whose dialog tag the reader
    /// mints itself (RFC 3261 §8.1.1.3, §8.2.6.2): [`parse_identity`], a `tag`
    /// header or URI parameter dropped. A `tag=` inside the userinfo or a
    /// `?`-header value is refused: the frozen header would carry a second tag.
    ///
    /// [`parse_identity`]: Self::parse_identity
    pub fn parse_untagged_identity(raw: &SipStr) -> Result<Self, SipParseError> {
        let addr = Self::parse_identity(raw)?;
        let uri = addr.uri();
        if uri.user().is_some_and(|u| u.to_ascii_lowercase().contains("tag=")) {
            return Err(SipParseError::new("a tag in the userinfo"));
        }
        if uri
            .escaped_headers()
            .any(|(_, v)| v.is_some_and(|v| v.to_ascii_lowercase().contains("tag=")))
        {
            return Err(SipParseError::new("a tag in a URI header"));
        }
        let uri = addr.uri().clone().without_param("tag");
        Ok(addr.without_param("tag").with_uri(uri))
    }

    /// The display name a value states EXACTLY as it is written: a quoted string
    /// keeps its quotes and its escapes, a bare one its own bytes. `None` where
    /// the value states none. [`display`](Self::display) is the unescaped reading;
    /// this is the spelling, for a projection that must reproduce what arrived.
    pub fn display_as_written(raw: &SipStr) -> Option<SipStr> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let start = skip_ws(bytes, 0);
        if byte_at(bytes, start)? == b'"' {
            let (_, after) = read_quoted(&value, start);
            return Some(value.subspan(start, after.saturating_sub(start)));
        }
        let open = index_of(bytes, b'<', start)?;
        let before = sub_trimmed(&value, start, open);
        (!before.as_str().is_empty()).then_some(before)
    }

    /// Render as `["display" ]<uri>*(";" param)`. The angle brackets are
    /// unconditional: they are always legal, and they keep a URI parameter from
    /// being read back as a header parameter.
    pub fn render(&self, out: &mut Wire) {
        if let Some(display) = &self.display {
            out.quoted(display.as_str());
            out.byte(b' ');
        }
        out.byte(b'<');
        self.uri.render(out);
        out.byte(b'>');
        self.params.render(out);
    }
}

/// Where a header's grammar lets its address stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrForm {
    /// `name-addr / addr-spec`: From, To, Contact and the identity headers.
    NameAddrOrSpec,
    /// `name-addr` alone: the route family (Route, Record-Route, Path,
    /// Service-Route), whose parameters a bare URI would swallow, and the
    /// bracketed RFC 6442 Geolocation value.
    NameAddrOnly,
}

/// What a bare `addr-spec` may hold that RFC 3261 §20 brackets elsewhere. A
/// `;` after a bare addr-spec always starts the header parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareAddrSpec {
    /// Contact, From and To (§20): a URI holding a `,` or `?` is bracketed.
    Bracketed,
    /// A list header: a `?` starts the URI's headers, and a `,` separates
    /// entries, so an entry holds none.
    UriHeaders,
    /// A single-valued header: a `?` starts the URI's headers, and a `,` is
    /// the URI's own (RFC 3515 §2.1 admits `addr-spec`).
    UriHeadersAndComma,
}

#[cfg(test)]
mod tests;
