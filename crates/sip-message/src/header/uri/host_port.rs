//! [`HostPort`] — the one host-and-optional-port shape.

use crate::sip_str::SipStr;

use super::super::wire::Wire;

/// A host with an optional explicit port. An IPv6 literal is held WITHOUT its
/// brackets; [`render`](Self::render) puts them back. A value held
/// [`whole`](Self::whole) renders as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    host: SipStr,
    port: Option<u16>,
    whole: bool,
}

impl HostPort {
    /// The port a peer reaches when the URI names none (RFC 3261 §19.1.2).
    pub const DEFAULT_PORT: u16 = 5060;

    pub fn new(host: impl Into<SipStr>, port: Option<u16>) -> Self {
        Self { host: host.into(), port, whole: false }
    }

    /// Text no host reader accepts, held whole as the host, with no port: it
    /// renders as written, never bracketed.
    pub fn whole(text: impl Into<SipStr>) -> Self {
        Self { host: text.into(), port: None, whole: true }
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
        let ipv6 = !self.whole && self.host.contains(':');
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
}
