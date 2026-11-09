//! Where a request to a [`Uri`] is sent: its `maddr`, else its host.

use super::read::{unbracketed, MADDR};
use super::{HostPort, Uri};

impl Uri {
    /// Where a request to this URI is sent (RFC 3261 §19.1.1, RFC 3263 §4):
    /// its [`maddr`](Self::maddr), else its host, at its own port or the
    /// default. Target-role parsing refuses a `maddr` that names no host, so
    /// this never fails. [`host_port`](Self::host_port) stays the authority.
    pub fn next_hop(&self) -> (&str, u16) {
        let port = self.authority.port_or_default();
        (self.maddr().unwrap_or_else(|| self.authority.host()), port)
    }

    /// The host a `sip` or `sips` URI's `maddr` names, an IPv6 reference
    /// without its brackets.
    pub fn maddr(&self) -> Option<&str> {
        if !self.is_sip_or_sips() {
            return None;
        }
        self.params.value(MADDR).map(unbracketed)
    }

    /// This URI once its `maddr` named the element reading it (RFC 3261 §16.4):
    /// without the `maddr`, a port other than the default, or a `transport`
    /// other than the UDP a `sip` URI defaults to, every other part kept.
    pub fn without_own_maddr(self) -> Self {
        let keeps_port = self.port().is_none_or(|port| port == HostPort::DEFAULT_PORT);
        let keeps_transport =
            self.params.value("transport").is_none_or(|t| t.eq_ignore_ascii_case("udp"));
        let mut uri = self.without_param(MADDR);
        if !keeps_port {
            uri.authority = HostPort::new(uri.authority.host().to_string(), None);
        }
        if !keeps_transport {
            uri.params.remove("transport");
        }
        uri
    }
}
