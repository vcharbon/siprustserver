//! What one routing URI states about where a message goes.

use crate::parser::custom::request_uri::request_uri_refusal;

/// What one URI states about where a message goes: the canonical text, its
/// authority, the host and effective port RFC 3263 §4 resolves it to, whether it
/// is a loose route (`;lr`, RFC 3261 §19.1.1), and whether a request can be
/// addressed to it at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriFacts {
    /// The URI rendered from its parts — the spelling two URIs compare on, so
    /// whitespace and the original layout never decide equality.
    pub uri: String,
    /// The host of its authority.
    pub host: String,
    /// The port the URI names, defaulted when it names none.
    pub port: u16,
    /// Where a request to it is sent: its `maddr`, else its host.
    pub next_hop_host: String,
    /// The URI carries `;lr`.
    pub loose: bool,
    /// The URI passes the checks a Request-URI is held to, so a request can
    /// name it as its target.
    pub addressable: bool,
}

impl UriFacts {
    /// Whether two URIs state one authority and one next hop.
    pub fn same_address(&self, other: &Self) -> bool {
        (&self.host, self.port, &self.next_hop_host)
            == (&other.host, other.port, &other.next_hop_host)
    }

    pub(super) fn of(uri: &crate::header::Uri) -> Self {
        let (host, port) = uri.host_port();
        UriFacts {
            uri: uri.to_string(),
            host: host.to_string(),
            port,
            next_hop_host: uri.next_hop().0.to_string(),
            loose: uri.is_loose_route(),
            addressable: request_uri_refusal(uri).is_ok(),
        }
    }
}
