//! [`Credentials`] — the authentication family (Authorization,
//! WWW-Authenticate and their proxy twins).
//!
//! Its parameters are comma-separated *inside one value*, so this header is
//! declared [`Folding::Opaque`]: a comma-splitting reader can never tear a
//! challenge in half.

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::kind::CredentialsKind;
use super::name::HeaderName;
use super::params::{parse_comma_params, ParamValue, Params};
use super::scan::{scan_until, skip_ws, sub};
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// An authentication scheme and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials<K: CredentialsKind> {
    scheme: SipStr,
    params: Params,
    kind: PhantomData<fn() -> K>,
}

impl<K: CredentialsKind> Credentials<K> {
    pub fn new(scheme: impl Into<SipStr>) -> Self {
        Self { scheme: scheme.into(), params: Params::new(), kind: PhantomData }
    }

    /// A `Digest` challenge or response.
    pub fn digest() -> Self {
        Self::new(SipStr::from_static("Digest"))
    }

    pub fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub fn is_digest(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("Digest")
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.params.get(name)
    }

    /// The text of one parameter — `realm`, `nonce`, `response`, …
    pub fn value(&self, name: &str) -> Option<&str> {
        self.params.value(name)
    }

    /// Set a parameter whose grammar is `quoted-string` (realm, nonce, uri,
    /// response, opaque, cnonce).
    pub fn with_quoted(mut self, name: impl Into<SipStr>, value: impl Into<SipStr>) -> Self {
        self.params.set(name, ParamValue::Quoted(value.into()));
        self
    }

    /// Set a parameter whose grammar is a bare token (algorithm, qop, nc,
    /// stale).
    pub fn with_token(mut self, name: impl Into<SipStr>, value: impl Into<SipStr>) -> Self {
        self.params.set(name, ParamValue::Token(value.into()));
        self
    }

    pub fn without_param(mut self, name: &str) -> Self {
        self.params.remove(name);
        self
    }
}

impl<K: CredentialsKind> HeaderValue for Credentials<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let start = skip_ws(bytes, 0);
        let scheme_end = scan_until(bytes, start, b" \t");
        let scheme = sub(&value, start, scheme_end);
        if scheme.is_empty() {
            return Err(SipParseError::new(format!(
                "{} has no auth-scheme: {:?}",
                K::name(),
                value.as_str()
            )));
        }
        let params = parse_comma_params(&value, scheme_end);
        Ok(Self { scheme, params, kind: PhantomData })
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.scheme.as_str());
        if !self.params.is_empty() {
            out.byte(b' ');
            self.params.render_comma_separated(out);
        }
    }
}

impl<K: CredentialsKind> std::fmt::Display for Credentials<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{Authorization, WwwAuthenticate};

    #[test]
    fn a_challenge_reads_its_parameters() {
        let raw = r#"Digest realm="atlanta.com", nonce="ab34", qop="auth", algorithm=MD5"#;
        let c = WwwAuthenticate::parse(&SipStr::owned(raw)).unwrap();
        assert!(c.is_digest());
        assert_eq!(c.value("realm"), Some("atlanta.com"));
        assert_eq!(c.value("ALGORITHM"), Some("MD5"));
        assert_eq!(c.to_wire(), raw);
    }

    #[test]
    fn the_family_never_comma_splits() {
        assert_eq!(Authorization::folding(), Folding::Opaque);
        let raw = SipStr::owned(r#"Digest realm="a", nonce="b""#);
        assert_eq!(Authorization::parse_line(&raw).unwrap().len(), 1);
    }

    #[test]
    fn a_response_is_its_own_builder() {
        let c = Authorization::digest()
            .with_quoted("username", "alice")
            .with_quoted("realm", "atlanta.com")
            .with_token("algorithm", "MD5");
        assert_eq!(c.to_wire(), r#"Digest username="alice", realm="atlanta.com", algorithm=MD5"#);
    }
}
