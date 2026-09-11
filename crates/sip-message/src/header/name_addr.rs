//! [`NameAddr`] — the `[display-name] <uri> *(";" param)` core every
//! address-valued header is built from.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::params::{parse_semicolon_params, ParamValue, Params, HEADER_PARAMS};
use super::scan::{index_of, read_quoted, skip_ws, sub_trimmed};
use super::uri::Uri;
use super::wire::Wire;

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

    /// Assemble from already-scanned parts — the parser's seam.
    pub(crate) fn from_parts(display: Option<SipStr>, uri: Uri, params: Params) -> Self {
        Self { display, uri, params }
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

    /// Read `[display-name] ("<" uri ">" / addr-spec) *(";" param)`.
    pub fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let len = bytes.len();
        let start = skip_ws(bytes, 0);

        let (display, uri_text, params_at) = if start < len && bytes[start] == b'"' {
            let (text, after) = read_quoted(&value, start);
            let open = skip_ws(bytes, after);
            let (uri_text, params_at) = angle_uri(&value, open)?;
            (Some(text), uri_text, params_at)
        } else if let Some(open) = index_of(bytes, b'<', start) {
            let before = sub_trimmed(&value, start, open);
            let display = if before.is_empty() { None } else { Some(before) };
            let (uri_text, params_at) = angle_uri(&value, open)?;
            (display, uri_text, params_at)
        } else {
            // addr-spec: every parameter after the URI is a HEADER parameter.
            let semi = index_of(bytes, b';', start).unwrap_or(len);
            (None, sub_trimmed(&value, start, semi), semi)
        };

        let uri = Uri::parse(&uri_text)?;
        let (params, _) = parse_semicolon_params(&value, params_at, &HEADER_PARAMS);
        Ok(Self { display, uri, params })
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

/// The URI inside `<...>` opening at byte `open`, plus the position of the
/// header parameters that follow it.
fn angle_uri(value: &SipStr, open: usize) -> Result<(SipStr, usize), SipParseError> {
    let bytes = value.as_bytes();
    if open >= bytes.len() || bytes[open] != b'<' {
        return Err(SipParseError::new(format!(
            "name-addr display name is not followed by <uri>: {:?}",
            value.as_str()
        )));
    }
    match index_of(bytes, b'>', open + 1) {
        Some(close) => Ok((sub_trimmed(value, open + 1, close), close + 1)),
        None => {
            Err(SipParseError::new(format!("name-addr has no closing `>`: {:?}", value.as_str())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> NameAddr {
        NameAddr::parse(&SipStr::owned(s)).unwrap_or_else(|e| panic!("{s}: {}", e.reason))
    }

    fn rendered(s: &str) -> String {
        let mut w = Wire::new();
        parse(s).render(&mut w);
        w.as_str().to_owned()
    }

    #[test]
    fn a_quoted_display_name_is_unescaped_and_requoted() {
        let n = parse(r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#);
        assert_eq!(n.display(), Some(r#"Alice "A""#));
        assert_eq!(n.params().value("tag"), Some("1928"));
        assert_eq!(
            rendered(r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#),
            r#""Alice \"A\"" <sip:alice@atlanta.com>;tag=1928"#
        );
    }

    #[test]
    fn an_unquoted_display_name_is_kept() {
        assert_eq!(parse("Bob <sip:bob@biloxi.com>").display(), Some("Bob"));
    }

    #[test]
    fn an_addr_spec_takes_its_parameters_as_header_parameters() {
        let n = parse("sip:alice@atlanta.com;tag=99");
        assert_eq!(n.uri().host(), "atlanta.com");
        assert!(n.uri().params().is_empty());
        assert_eq!(n.params().value("tag"), Some("99"));
        assert_eq!(rendered("sip:alice@atlanta.com;tag=99"), "<sip:alice@atlanta.com>;tag=99");
    }

    #[test]
    fn a_bracketed_uri_keeps_its_own_parameters() {
        let n = parse("<sip:p.example;lr>;tag=7");
        assert!(n.uri().is_loose_route());
        assert_eq!(n.params().value("tag"), Some("7"));
    }

    #[test]
    fn an_unclosed_angle_bracket_is_an_error() {
        assert!(NameAddr::parse(&SipStr::owned("<sip:a@h")).is_err());
    }
}
