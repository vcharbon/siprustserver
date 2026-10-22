//! What one routing URI states about where a message goes.

use crate::parser::custom::request_uri::request_uri_refusal;

/// What one URI states about where a message goes: the canonical text, the
/// host and effective port RFC 3263 §4 resolves it to, whether it is a loose
/// route (`;lr`, RFC 3261 §19.1.1), and whether a request can be addressed to
/// it at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriFacts {
    /// The URI rendered from its parts — the spelling two URIs compare on, so
    /// whitespace and the original layout never decide equality.
    pub uri: String,
    pub host: String,
    /// The port the URI names, defaulted when it names none.
    pub port: u16,
    /// The URI carries `;lr`.
    pub loose: bool,
    /// The URI passes the checks a Request-URI is held to, so a request can
    /// name it as its target.
    pub addressable: bool,
}

impl UriFacts {
    pub(super) fn of(uri: &crate::header::Uri) -> Self {
        let (host, port) = uri.host_port();
        UriFacts {
            uri: uri.to_string(),
            host: host.to_string(),
            port,
            loose: uri.is_loose_route(),
            addressable: request_uri_refusal(uri).is_ok(),
        }
    }
}

/// The Request-URI's routing facts when it names a host RFC 3263 §4
/// resolves; `None` for a response, a URI no reader accepts, or one of a
/// scheme read opaque (`urn:`, `http:` …).
pub fn request_uri_target(raw: &[u8]) -> Option<UriFacts> {
    let text = super::request_uri(raw)?;
    let uri = crate::header::Uri::parse(&crate::sip_str::SipStr::owned(&text)).ok()?;
    uri.opaque_part().is_none().then(|| UriFacts::of(&uri))
}
