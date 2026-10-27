//! [`Credentials`] — the authentication family (Authorization,
//! WWW-Authenticate and their proxy twins).
//!
//! Its parameters are comma-separated *inside one value*, so this header is
//! declared [`Folding::Opaque`]: a comma-splitting reader can never tear a
//! challenge in half.
//!
//! A value reads as RFC 3261 §25.1 writes it: an `auth-scheme` token, then
//! `auth-param`s (`token EQUAL ( token / quoted-string )`, comma-separated)
//! or one RFC 7235 §2.1 `token68`, kept as a single flag. Anything else is
//! refused, so a value read renders to text that reads back as itself.

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use crate::char_class::is_token_char;

use super::kind::CredentialsKind;
use super::name::HeaderName;
use super::params::{ParamValue, Params};
use super::scan::{byte_at, read_quoted, skip_ws, sub};
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
        let value = without_lws(raw);
        let bytes = value.as_bytes();
        let refused =
            |what: &str| SipParseError::new(format!("{} {what}: {:?}", K::name(), value.as_str()));
        let scheme_end = token_end(bytes, 0);
        if scheme_end == 0 {
            return Err(refused("has no auth-scheme"));
        }
        let scheme = sub(&value, 0, scheme_end);
        let rest = skip_ws(bytes, scheme_end);
        let params = if rest == bytes.len() {
            Params::new()
        } else if rest == scheme_end {
            return Err(refused("has no space after its auth-scheme"));
        } else if bytes.split_at_checked(rest).is_some_and(|(_, tail)| is_token68(tail)) {
            let mut one = Params::new();
            one.push(sub(&value, rest, bytes.len()), ParamValue::Flag);
            one
        } else {
            auth_params(&value, rest).ok_or_else(|| refused("has a malformed auth-param"))?
        };
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

/// `raw` without the SP and HTAB around it.
fn without_lws(raw: &SipStr) -> SipStr {
    let bytes = raw.as_bytes();
    let start = skip_ws(bytes, 0);
    let trailing = bytes.iter().rev().take_while(|&&b| matches!(b, b' ' | b'\t')).count();
    sub(raw, start, bytes.len().saturating_sub(trailing).max(start))
}

/// The end of the token starting at byte `i`: `i` itself when none does.
fn token_end(bytes: &[u8], i: usize) -> usize {
    i + bytes.iter().skip(i).take_while(|&&b| is_token_char(b)).count()
}

/// `token68 = 1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`
/// (RFC 7235 §2.1), the whole of `rest`.
fn is_token68(rest: &[u8]) -> bool {
    let body = rest.iter().take_while(|&&b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b));
    let body = body.count();
    body > 0 && rest.iter().skip(body).all(|&b| b == b'=')
}

/// The position after the `"` closing the `quoted-string` opened at byte
/// `open` (RFC 3261 §25.1): `qdtext` is HTAB, SP or any byte but a control,
/// `"` and `\`; a `quoted-pair` escapes one ASCII byte but CR and LF. `None`
/// when a byte breaks it or nothing closes it.
fn quoted_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    loop {
        match byte_at(bytes, i)? {
            b'"' => return Some(i + 1),
            b'\\' => match byte_at(bytes, i + 1)? {
                b'\r' | b'\n' => return None,
                escaped if escaped.is_ascii() => i += 2,
                _ => return None,
            },
            b'\t' => i += 1,
            b if b < 0x20 || b == 0x7f => return None,
            _ => i += 1,
        }
    }
}

/// The `auth-param`s from byte `i` to the end of `base`, comma-separated
/// with optional whitespace; `None` at the first byte off the grammar.
fn auth_params(base: &SipStr, mut i: usize) -> Option<Params> {
    let bytes = base.as_bytes();
    let mut params = Params::new();
    loop {
        let name_end = token_end(bytes, i);
        if name_end == i {
            return None;
        }
        let name = sub(base, i, name_end);
        i = skip_ws(bytes, name_end);
        if byte_at(bytes, i) != Some(b'=') {
            return None;
        }
        i = skip_ws(bytes, i + 1);
        if byte_at(bytes, i) == Some(b'"') {
            let close = quoted_end(bytes, i)?;
            let (text, _) = read_quoted(base, i);
            params.push(name, ParamValue::Quoted(text));
            i = close;
        } else {
            let value_end = token_end(bytes, i);
            if value_end == i {
                return None;
            }
            params.push(name, ParamValue::Token(sub(base, i, value_end)));
            i = value_end;
        }
        i = skip_ws(bytes, i);
        match byte_at(bytes, i) {
            None => return Some(params),
            Some(b',') => i = skip_ws(bytes, i + 1),
            Some(_) => return None,
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
    fn parameters_split_on_commas_not_semicolons() {
        let c =
            Authorization::parse(&SipStr::owned(r#"Digest realm="a;b", nonce=xyz, stale=FALSE"#))
                .unwrap();
        assert_eq!(c.params().len(), 3);
        assert_eq!(c.value("realm"), Some("a;b"));
        assert_eq!(c.value("nonce"), Some("xyz"));
    }

    #[test]
    fn the_family_never_comma_splits() {
        assert_eq!(Authorization::folding(), Folding::Opaque);
        let raw = SipStr::owned(r#"Digest realm="a", nonce="b""#);
        assert_eq!(Authorization::parse_line(&raw).unwrap().len(), 1);
    }

    /// A value off the grammar is refused: a scheme that is no token, a
    /// quoted-string left open, text after a closed one, a parameter with no
    /// value, an empty list element, a bare control byte or an escaped
    /// non-ASCII one in a quoted-string, whitespace other than SP and HTAB.
    #[test]
    fn a_value_off_the_grammar_is_refused() {
        for raw in [
            r#"ce", realm="atlanta.com""#,
            r#"Digest realm="atlanta.com"#,
            r#"Digest cnonce="0a4f113b"49393a0, opaque="5ccc""#,
            r#"Digest realm="atlanta.com", stale"#,
            r#"Digest realm="atlanta.com",, nonce="ab34""#,
            r#"Digest realm=, nonce="ab34""#,
            "Digest\"realm",
            "Digest realm=\"a\u{1}b\"",
            "Digest realm=\"\\é\"",
            "Digest realm=x\u{a0}",
            "\u{b}Digest realm=x",
        ] {
            assert!(Authorization::parse(&SipStr::owned(raw)).is_err(), "{raw}");
        }
    }

    /// A value read renders to text that reads back to the same rendering.
    #[test]
    fn a_value_read_renders_to_itself() {
        for raw in [
            r#"Digest username="bob", realm="a\"b", nc=00000001 , qop=auth"#,
            "Bearer dXNlcjpwYXNzd29yZA==",
            "Negotiate",
            "Digest realm=\"a\\\u{7}b\"",
        ] {
            let read = Authorization::parse(&SipStr::owned(raw)).unwrap();
            let wire = read.to_wire();
            let again = Authorization::parse(&SipStr::owned(&wire)).unwrap();
            assert_eq!(again.to_wire(), wire, "{raw}");
        }
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
