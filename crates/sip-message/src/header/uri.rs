//! [`Uri`] — the one parsed URI shape, and [`HostPort`] — the one
//! host-and-optional-port shape.
//!
//! Every URI a message carries (Request-URI, the URI inside a name-addr, a Via
//! sent-by) is this type: a reader asks the value for its host and port instead
//! of peeling a string.

use std::borrow::Cow;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::params::{ParamValue, Params};
use super::scan::{byte_at, clamped, digits_end, index_of, parse_port, scan_until, sub};
use super::wire::Wire;

mod read;
pub(crate) use read::host_refusal;

/// What a URI's host is for, the one axis on which its grammar varies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriRole {
    /// A host a request is sent to or routed on (Request-URI, Contact, Route,
    /// Record-Route, Via, Refer-To): RFC 3261's `hostname`, its top label
    /// starting with a letter, so no resolver reads it as an IPv4 literal.
    Target,
    /// A host that names a party and is never resolved (From, To, the identity
    /// headers): the top label may also start with a digit (RFC 1123 §2.1).
    Identity,
}

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
    /// after it. An IPv6 host must come bracketed (RFC 3261 §19.1.1): an
    /// unbracketed one is refused, never read as the text before its first
    /// colon.
    pub(crate) fn parse_at(base: &SipStr, i: usize) -> Result<(Self, usize), SipParseError> {
        let bytes = base.as_bytes();
        let (host, mut j) = if byte_at(bytes, i) == Some(b'[') {
            match index_of(bytes, b']', i + 1) {
                Some(close) => (sub(base, i + 1, close), close + 1),
                None => return Err(SipParseError::new("unclosed IPv6 reference")),
            }
        } else {
            // A second colon inside the authority is an IPv6 literal written
            // without its brackets. Keeping the text before the first colon
            // would name a host the peer never wrote — and a router that
            // resolves it is worse than one that refuses the URI.
            let authority = scan_until(bytes, i, b";,> \t?");
            let head = bytes.get(..authority).unwrap_or(bytes);
            let colon = index_of(head, b':', i);
            if colon.is_some_and(|c| index_of(head, b':', c + 1).is_some()) {
                return Err(SipParseError::new(format!(
                    "unbracketed IPv6 host (RFC 3261 §19.1.1 requires []): {:?}",
                    clamped(base.as_str(), i, authority)
                )));
            }
            let end = colon.unwrap_or(authority);
            (sub(base, i, end), end)
        };
        let mut port = None;
        if byte_at(bytes, j) == Some(b':') {
            let digits = digits_end(bytes, j + 1);
            if digits > j + 1 {
                port = Some(parse_port(clamped(base.as_str(), j + 1, digits))?);
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
    /// The `?a=b&c=d` escaped-header list (RFC 3261 §19.1.1), verbatim. A
    /// sip/sips pair always carries a value; a pair of another scheme written
    /// without its `=` carries none and renders back without one.
    headers: Vec<(SipStr, Option<SipStr>)>,
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
        let text: SipStr = text.into();
        Self {
            scheme: SipStr::EMPTY,
            user: None,
            authority: HostPort::new(text.clone(), None),
            params: Params::new(),
            headers: Vec::new(),
            source: Some(text),
        }
    }

    /// Whether this value could not be read as a URI and is kept whole — the
    /// state [`opaque`](Self::opaque) puts it in. A router must not resolve one:
    /// its host and port mean nothing.
    pub fn is_opaque(&self) -> bool {
        self.scheme.is_empty()
    }

    pub fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    /// Whether a raw value already opens with a URI scheme (RFC 3261 §25.1:
    /// `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` then `":"`). A
    /// colon alone does not say so — `anonymous@10.0.0.1:5060` carries a PORT
    /// colon, and reading it as a scheme mints a name-addr holding no URI.
    pub fn value_has_scheme(value: &str) -> bool {
        let bytes = value.trim().as_bytes();
        let Some(&first) = bytes.first() else { return false };
        if !first.is_ascii_alphabetic() {
            return false;
        }
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b':' => return i > 0,
                b if b.is_ascii_alphanumeric() => {}
                b'+' | b'-' | b'.' => {}
                _ => return false,
            }
        }
        false
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

    /// The escaped-header pairs in wire order, undecoded. A pair the peer wrote
    /// without an `=` yields `None` — a name carrying no value, kept apart from
    /// one carrying an empty one.
    pub fn escaped_headers(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.headers.iter().map(|(k, v)| (k.as_str(), v.as_ref().map(SipStr::as_str)))
    }

    /// One escaped header, percent-decoded. `None` when absent, written without
    /// a value, or malformed.
    pub fn escaped_header(&self, name: &str) -> Option<String> {
        let raw = self
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.as_ref())?
            .as_str();
        crate::parser::custom::structured_headers::decode_uri_component(raw).ok()
    }

    /// The text this URI was read from, or `None` when it was built from parts
    /// or has been updated since.
    pub fn source(&self) -> Option<&str> {
        self.source.as_ref().map(SipStr::as_str)
    }

    /// The wire text of this URI: borrowed while it still carries the bytes it
    /// was read from, rendered from its parts otherwise. The read a consumer
    /// wants when it displays or text-matches a URI it never edits.
    pub fn text(&self) -> Cow<'_, str> {
        match &self.source {
            Some(source) => Cow::Borrowed(source.as_str()),
            None => {
                let mut out = Wire::new();
                self.render(&mut out);
                Cow::Owned(out.as_str().to_owned())
            }
        }
    }

    /// The canonical USER identity this URI names, for comparison across
    /// entities: the userinfo of a sip URI with its `;`-params (`verstat`,
    /// `phone-context`) dropped, or the subscriber part of a tel URI. A
    /// phone-shaped identity — optional `+` then digits, RFC 3966 visual
    /// separators `-`, `.`, `(`, `)` allowed — normalizes by dropping the
    /// separators, so `tel:+1-408-555-1212` and `sip:+14085551212@host` name
    /// the same subscriber. Scheme, host, port and parameters never
    /// participate. `None` for a userless sip URI.
    pub fn user_identity(&self) -> Option<String> {
        // tel: has no userinfo — the subscriber number sits in the host slot.
        let raw = if self.scheme.eq_ignore_ascii_case("tel") {
            self.authority.host()
        } else {
            self.user()?
        };
        let user = raw.split(';').next().unwrap_or("");
        if user.is_empty() {
            return None;
        }
        let stripped: String =
            user.chars().filter(|c| !matches!(c, '-' | '.' | '(' | ')')).collect();
        let digits = stripped.strip_prefix('+').unwrap_or(&stripped);
        let phone_shaped = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        Some(if phone_shaped { stripped } else { user.to_string() })
    }

    /// The dialled DIGITS of this URI's user identity: every non-digit dropped,
    /// then one leading `00` or `0` (international / trunk prefix) removed, so
    /// `tel:+1-555-0123`, `sip:0015550123@h` and `sip:15550123@h;npdi` all read `15550123`.
    /// `None` when the identity carries no digit at all (`sip:anonymous@…`) —
    /// a caller comparing subscribers falls back to [`Uri::user_identity`].
    pub fn user_digits(&self) -> Option<String> {
        let identity = self.user_identity()?;
        let digits: String = identity.chars().filter(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        let trimmed = digits.strip_prefix("00").or_else(|| digits.strip_prefix('0'));
        Some(trimmed.unwrap_or(&digits).to_string())
    }

    /// Whether two URIs name the same user identity. The comparison is
    /// BYTE-EXACT because the user part is case-sensitive (RFC 3261 §19.1.4);
    /// phone-shaped identities are already normalized, so case plays no part
    /// in them. `false` when either side names no user.
    pub fn same_user_identity(&self, other: &Uri) -> bool {
        match (self.user_identity(), other.user_identity()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
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

    /// This URI with its `?a=b` escaped-header list dropped — what a
    /// Request-URI may carry (RFC 3261 §19.1.1 forbids escaped headers there,
    /// so a URI taken from Refer-To or a redirect Contact is reduced by this
    /// before it becomes one).
    pub fn without_escaped_headers(mut self) -> Self {
        self.headers.clear();
        self.normalized()
    }

    /// Read a whole URI, surrounding whitespace aside, as a [`UriRole::Target`].
    /// The value must carry a scheme colon; anything looser is
    /// [`opaque`](Self::opaque) territory, not a URI. A `sip` / `sips` URI is
    /// held to the RFC 3261 §25.1 grammar byte for byte; another scheme to the
    /// URI character set (ADR-0053).
    pub fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        Self::parse_as(raw, UriRole::Target)
    }

    /// [`parse`](Self::parse) in the given role.
    pub fn parse_as(raw: &SipStr, role: UriRole) -> Result<Self, SipParseError> {
        Self::read_exact(&raw.trimmed(), role)
    }

    /// Read all of `value` as one URI, whitespace included: the reading of a
    /// URI whose surroundings already delimit it (a Request-URI, the inside of
    /// `<...>`), where a space is a byte of the URI and refuses it.
    pub(crate) fn read_exact(value: &SipStr, role: UriRole) -> Result<Self, SipParseError> {
        let read::UriParts { scheme, user, authority, params, headers } = read::read(value, role)?;
        Ok(Self { scheme, user, authority, params, headers, source: Some(value.clone()) })
    }

    /// [`parse`](Self::parse), falling back to [`opaque`](Self::opaque): text no
    /// reader accepts is **carried whole** rather than refused.
    ///
    /// Two lanes only, and in both of them carrying the bytes forward verbatim
    /// is the feature:
    ///   - a **reader** converting an already-gated message, where losing what
    ///     the peer wrote is worse than holding an address this stack cannot
    ///     take apart (the parser's own strict gates decide admissibility);
    ///   - **test-side replay emission**, where a scenario states an address as
    ///     text and the wire must carry that spelling back.
    ///
    /// NEVER on a path that routes, addresses or answers a call. There a value
    /// that does not read is an [`Err`] the caller propagates to a seam that can
    /// act on it — the opaque URI's host is the whole raw text, so routing on
    /// one dials an address nobody named.
    pub fn parse_or_verbatim(raw: &SipStr) -> Self {
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
            if let Some(value) = value {
                out.byte(b'=');
                out.str(value.as_str());
            }
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
mod tests;
