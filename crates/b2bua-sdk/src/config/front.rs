//! Where a worker stands toward its peers: alone, or behind the SIP front
//! proxy. The mode decides where b-leg requests are sent and what address
//! every Contact the worker allocates names.

use super::B2buaConfig;

/// The worker's deployment mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Front {
    /// Peers reach the worker at its own address: every Contact names
    /// [`B2buaConfig::sip_local_ip`] / [`B2buaConfig::sip_local_port`] and
    /// b-leg requests go straight to the callee.
    Standalone,
    /// Behind the SIP front proxy.
    Proxied {
        /// The proxy's worker-facing `(host, port)`: every b-leg request is
        /// sent there with a preloaded plain `Route: <sip:host:port;lr>`, which
        /// the proxy classifies worker-outbound (ADR-0048). The R-URI / remote
        /// target stays the callee (RFC 3261 §16.12).
        outbound_proxy: (String, u16),
        /// The proxy's peer-facing `(host, port)`, the host of every Contact
        /// the worker allocates: the remote target a peer holds names the
        /// cluster, never one worker, so every copy of a call states the same
        /// Contact (ADR-0055 §14).
        contact_address: (String, u16),
    },
}

impl Front {
    /// Behind a proxy with one face: peers and workers reach it at the same
    /// `(host, port)`, which is both the outbound proxy and the contact
    /// address.
    pub fn single_face(host: &str, port: u16) -> Self {
        Front::Proxied {
            outbound_proxy: (host.to_string(), port),
            contact_address: (host.to_string(), port),
        }
    }

    /// The proxy every b-leg request is sent through; `None` standalone.
    pub fn outbound_proxy(&self) -> Option<&(String, u16)> {
        match self {
            Front::Standalone => None,
            Front::Proxied { outbound_proxy, .. } => Some(outbound_proxy),
        }
    }
}

impl B2buaConfig {
    /// The `(host, port)` every Contact this worker allocates names: the
    /// proxy's peer-facing address behind the proxy, the worker's own
    /// standalone.
    pub fn contact_address(&self) -> (&str, u16) {
        match &self.front {
            Front::Standalone => (&self.sip_local_ip, self.sip_local_port),
            Front::Proxied { contact_address, .. } => (&contact_address.0, contact_address.1),
        }
    }
}
