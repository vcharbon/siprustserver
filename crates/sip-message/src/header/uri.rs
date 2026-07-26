//! [`Uri`] — the one parsed URI shape, and [`HostPort`] — the one
//! host-and-optional-port shape.
//!
//! Every URI a message carries (Request-URI, the URI inside a name-addr, a Via
//! sent-by) is this type: a reader asks the value for its host and port instead
//! of peeling a string.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::params::{parse_semicolon_params, ParamValue, Params, URI_PARAMS};
use super::scan::{digits_end, index_of, parse_port, scan_until, sub, sub_lower};
use super::wire::Wire;

/// A host with an optional explicit port. An IPv6 literal is held WITHOUT its
/// brackets; [`render`](Self::render) puts them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    host: SipStr,
    port: Option<u16>,
}

impl HostPort {
    /// The port a peer reaches when the URI names none (RFC 3261 §19.1.2).
    pub const DEFAULT_PORT: u16 = 5060;

    pub fn new(host: impl Into<SipStr>, port: Option<u16>) -> Self {
        Self { host: host.into(), port }
    }

    pub fn host(&self) -> &str {
        self.host.as_str()
    }

    /// The port as written — `None` when the URI relies on the default.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The port to send to.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(Self::DEFAULT_PORT)
    }

    /// Host and effective port — what a transport wants.
    pub fn pair(&self) -> (&str, u16) {
        (self.host(), self.port_or_default())
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    pub fn render(&self, out: &mut Wire) {
        let ipv6 = self.host.contains(':');
        if ipv6 {
            out.byte(b'[');
        }
        out.str(self.host.as_str());
        if ipv6 {
            out.byte(b']');
        }
        if let Some(port) = self.port {
            out.byte(b':');
            out.num(port as u64);
        }
    }

    /// Read `host[:port]` from byte `i`; yields the value and the position
    /// after it.
    pub(crate) fn parse_at(base: &SipStr, i: usize) -> Result<(Self, usize), SipParseError> {
        let bytes = base.as_bytes();
        let len = bytes.len();
        let (host, mut j) = if i < len && bytes[i] == b'[' {
            match index_of(bytes, b']', i + 1) {
                Some(close) => (sub(base, i + 1, close), close + 1),
                None => return Err(SipParseError::new("unclosed IPv6 reference")),
            }
        } else {
            let end = scan_until(bytes, i, b":;,> \t?");
            (sub(base, i, end), end)
        };
        let mut port = None;
        if j < len && bytes[j] == b':' {
            let digits = digits_end(bytes, j + 1);
            if digits > j + 1 {
                port = Some(parse_port(&base.as_str()[j + 1..digits])?);
            }
            j = digits;
        }
        Ok((Self { host, port }, j))
    }
}

/// A SIP/SIPS/tel/any-scheme URI.
///
/// A URI read off the wire renders the bytes it was read from until an update
/// touches it: RFC 3261 §16.6 forbids a proxy rewriting a Request-URI it is not
/// retargeting, and §9.1 / §17.1.1.3 require the CANCEL and the non-2xx ACK to
/// carry the INVITE's Request-URI unchanged. Identity is the parsed parts, so
/// two URIs spelled differently but meaning the same thing compare equal.
#[derive(Debug, Clone, Eq)]
pub struct Uri {
    scheme: SipStr,
    user: Option<SipStr>,
    authority: HostPort,
    params: Params,
    /// The `?a=b&c=d` escaped-header list (RFC 3261 §19.1.1), verbatim.
    headers: Vec<(SipStr, SipStr)>,
    /// The text this URI was read from, dropped by the first update.
    source: Option<SipStr>,
}

impl PartialEq for Uri {
    fn eq(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.user == other.user
            && self.authority == other.authority
            && self.params == other.params
            && self.headers == other.headers
    }
}

/// The scheme this stack originates URIs under.
const SIP_SCHEME: SipStr = SipStr::from_static("sip");

impl Uri {
    /// A `sip:host` URI.
    pub fn sip(host: impl Into<SipStr>) -> Self {
        Self::new(SIP_SCHEME, None::<SipStr>, HostPort::new(host, None))
    }

    /// A `sip:user@host` URI.
    pub fn sip_user(user: impl Into<SipStr>, host: impl Into<SipStr>) -> Self {
        Self::new(SIP_SCHEME, Some(user), HostPort::new(host, None))
    }

    pub fn new(
        scheme: impl Into<SipStr>,
        user: Option<impl Into<SipStr>>,
        authority: HostPort,
    ) -> Self {
        Self {
            scheme: scheme.into(),
            user: user.map(Into::into),
            authority,
            params: Params::new(),
            headers: Vec::new(),
            source: None,
        }
    }

    /// A value that could not be read as a URI, kept whole so a reader can
    /// still see what the peer sent.
    pub fn opaque(text: impl Into<SipStr>) -> Self {
        Self {
            scheme: SipStr::EMPTY,
            user: None,
            authority: HostPort::new(text, None),
            params: Params::new(),
            headers: Vec::new(),
            source: None,
        }
    }

    pub fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub fn is_secure(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("sips")
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_ref().map(SipStr::as_str)
    }

    pub fn host(&self) -> &str {
        self.authority.host()
    }

    pub fn port(&self) -> Option<u16> {
        self.authority.port()
    }

    /// Host and effective port — the replacement for peeling a URI string down
    /// to a transport destination.
    pub fn host_port(&self) -> (&str, u16) {
        self.authority.pair()
    }

    pub fn authority(&self) -> &HostPort {
        &self.authority
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.params.get(name)
    }

    /// Whether this URI carries `;lr` — a loose router (RFC 3261 §19.1.1).
    pub fn is_loose_route(&self) -> bool {
        self.params.has("lr")
    }

    /// The escaped-header values in wire order, undecoded.
    pub fn escaped_headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// One escaped header, percent-decoded. `None` when absent or malformed.
    pub fn escaped_header(&self, name: &str) -> Option<String> {
        let raw = self
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())?;
        crate::parser::custom::structured_headers::decode_uri_component(raw).ok()
    }

    /// The text this URI was read from, or `None` when it was built from parts
    /// or has been updated since.
    pub fn source(&self) -> Option<&str> {
        self.source.as_ref().map(SipStr::as_str)
    }

    /// This URI rendered from its parsed parts instead of the text it was read
    /// from — canonical spelling, for a caller that wants one.
    pub fn normalized(mut self) -> Self {
        self.source = None;
        self
    }

    pub fn with_user(mut self, user: impl Into<SipStr>) -> Self {
        self.user = Some(user.into());
        self.normalized()
    }

    pub fn without_user(mut self) -> Self {
        self.user = None;
        self.normalized()
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.authority = self.authority.with_port(port);
        self.normalized()
    }

    pub fn with_param(mut self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        self.params.set(name, value);
        self.normalized()
    }

    pub fn with_flag(self, name: impl Into<SipStr>) -> Self {
        self.with_param(name, ParamValue::Flag)
    }

    pub fn without_param(mut self, name: &str) -> Self {
        self.params.remove(name);
        self.normalized()
    }

    /// Read a whole URI. The value must carry a scheme colon; anything looser
    /// is [`opaque`](Self::opaque) territory, not a URI.
    pub fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let len = bytes.len();
        let colon = index_of(bytes, b':', 0)
            .ok_or_else(|| SipParseError::new(format!("URI has no scheme: {:?}", value.as_str())))?;
        let scheme = sub_lower(&value, 0, colon);
        let mut i = colon + 1;

        // Userinfo may itself contain `;` (RFC 3261 §19.1.1 escaped user), so
        // the `@` search runs to the end of the value rather than stopping at
        // the first parameter.
        let at = scan_until(bytes, i, b"@>");
        let user = if at < len && bytes[at] == b'@' {
            let u = sub(&value, i, at);
            i = at + 1;
            Some(u)
        } else {
            None
        };

        let (authority, after_host) = HostPort::parse_at(&value, i)?;
        let (params, after_params) = parse_semicolon_params(&value, after_host, &URI_PARAMS);

        let mut headers = Vec::new();
        if after_params < len && bytes[after_params] == b'?' {
            for pair in value.as_str()[after_params + 1..].split('&') {
                let Some(eq) = pair.find('=') else { continue };
                headers.push((value.reslice(&pair[..eq]), value.reslice(&pair[eq + 1..])));
            }
        }

        Ok(Self { scheme, user, authority, params, headers, source: Some(value) })
    }

    /// [`parse`](Self::parse), falling back to [`opaque`](Self::opaque) — the
    /// total reading a converter from an already-validated message needs.
    pub fn parse_or_opaque(raw: &SipStr) -> Self {
        Self::parse(raw).unwrap_or_else(|_| Self::opaque(raw.clone()))
    }

    pub fn render(&self, out: &mut Wire) {
        if let Some(source) = &self.source {
            out.str(source.as_str());
            return;
        }
        if self.scheme.is_empty() {
            out.str(self.authority.host());
            return;
        }
        out.str(self.scheme.as_str());
        out.byte(b':');
        if let Some(user) = &self.user {
            out.str(user.as_str());
            out.byte(b'@');
        }
        self.authority.render(out);
        self.params.render(out);
        for (i, (name, value)) in self.headers.iter().enumerate() {
            out.byte(if i == 0 { b'?' } else { b'&' });
            out.str(name.as_str());
            out.byte(b'=');
            out.str(value.as_str());
        }
    }
}

impl std::fmt::Display for Uri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut w = Wire::new();
        self.render(&mut w);
        f.write_str(w.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        Uri::parse(&SipStr::owned(s)).unwrap_or_else(|e| panic!("{s}: {}", e.reason))
    }

    #[test]
    fn a_full_uri_reads_every_part() {
        let u = uri("sips:alice@atlanta.com:5061;transport=tls;lr?Replaces=abc");
        assert_eq!(u.scheme(), "sips");
        assert!(u.is_secure());
        assert_eq!(u.user(), Some("alice"));
        assert_eq!(u.host_port(), ("atlanta.com", 5061));
        assert_eq!(u.param("transport").and_then(ParamValue::as_str), Some("tls"));
        assert!(u.is_loose_route());
        assert_eq!(u.escaped_header("replaces").as_deref(), Some("abc"));
    }

    #[test]
    fn a_portless_uri_reports_the_default() {
        assert_eq!(uri("sip:bob@biloxi.com").host_port(), ("biloxi.com", 5060));
        assert_eq!(uri("sip:bob@biloxi.com").port(), None);
    }

    #[test]
    fn an_ipv6_literal_loses_and_regains_its_brackets() {
        let u = uri("sip:[2001:db8::1]:5080");
        assert_eq!(u.host(), "2001:db8::1");
        assert_eq!(u.port(), Some(5080));
        assert_eq!(u.to_string(), "sip:[2001:db8::1]:5080");
    }

    #[test]
    fn an_out_of_range_port_is_rejected() {
        assert!(Uri::parse(&SipStr::owned("sip:h:88161")).is_err());
    }

    #[test]
    fn a_schemeless_value_is_opaque_not_a_uri() {
        assert!(Uri::parse(&SipStr::owned("*")).is_err());
        assert_eq!(Uri::parse_or_opaque(&SipStr::owned("*")).to_string(), "*");
    }

    #[test]
    fn an_unedited_uri_renders_the_bytes_it_was_read_from() {
        // Every part the field renderer would normalize away: the scheme case,
        // an escaped-header pair with no `=`, and a parameter order.
        let text = "SIP:Bob@biloxi.com:5060;Transport=TCP?Subject&Replaces=abc";
        assert_eq!(uri(text).to_string(), text);
        assert_eq!(uri(text).scheme(), "sip");
    }

    #[test]
    fn an_edited_uri_renders_from_its_parts() {
        let edited = uri("SIP:bob@biloxi.com").with_port(5070);
        assert_eq!(edited.to_string(), "sip:bob@biloxi.com:5070");
        assert_eq!(uri("SIP:bob@biloxi.com").normalized().to_string(), "sip:bob@biloxi.com");
    }

    #[test]
    fn spelling_is_not_identity() {
        assert_eq!(uri("SIP:bob@biloxi.com"), uri("sip:bob@biloxi.com"));
        assert_eq!(uri("sip:bob@biloxi.com"), Uri::sip_user("bob", "biloxi.com"));
    }

    #[test]
    fn functional_updates_compose() {
        let u = Uri::sip_user("bob", "biloxi.com").with_port(5070).with_flag("lr");
        assert_eq!(u.to_string(), "sip:bob@biloxi.com:5070;lr");
        assert_eq!(u.without_param("lr").without_user().to_string(), "sip:biloxi.com:5070");
    }
}
