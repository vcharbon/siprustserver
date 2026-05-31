//! [`ProxyAddr`] — the proxy's `(host, port)` policy address.
//!
//! Port of the source `SocketAddr` value type (`RoutingStrategy.ts` — a
//! `{ host: string; port: number }` pair, NOT `std::net::SocketAddr`). It is
//! kept as a *string host + u16 port* on purpose: the stickiness cookie's
//! `ForwardAll` form is literally `target=host:port`, and a worker registry
//! entry is addressed by host string. The proxy resolves a `ProxyAddr` to a
//! real [`std::net::SocketAddr`] only at the `send_to` boundary
//! ([`ProxyAddr::to_socket_addr`]).

use std::net::SocketAddr;

/// A downstream UDP target — the `host:port` pair the core forwards to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyAddr {
    pub host: String,
    pub port: u16,
}

impl ProxyAddr {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port }
    }

    /// Parse a `host:port` string (the cookie / registry wire form). Returns
    /// `None` on a missing/empty host or an unparseable port. IPv6 literals are
    /// not split here (the source registry uses host names / IPv4); a bracketed
    /// `[::1]:5060` form is left for the registry parser if ever needed.
    pub fn parse(s: &str) -> Option<Self> {
        let (host, port) = s.rsplit_once(':')?;
        if host.is_empty() {
            return None;
        }
        let port: u16 = port.parse().ok()?;
        Some(Self { host: host.to_string(), port })
    }

    /// Resolve to a real socket address for `send_to`. Accepts an IP-literal
    /// host directly; a non-numeric host fails (the simulated fabric and the
    /// tests address everything by IP literal, matching the source).
    pub fn to_socket_addr(&self) -> Option<SocketAddr> {
        format!("{}:{}", self.host, self.port).parse().ok()
    }
}

impl std::fmt::Display for ProxyAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl From<SocketAddr> for ProxyAddr {
    fn from(a: SocketAddr) -> Self {
        Self { host: a.ip().to_string(), port: a.port() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_with_display() {
        let a = ProxyAddr::parse("10.0.0.2:5070").unwrap();
        assert_eq!(a, ProxyAddr::new("10.0.0.2", 5070));
        assert_eq!(a.to_string(), "10.0.0.2:5070");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(ProxyAddr::parse("nope").is_none());
        assert!(ProxyAddr::parse(":5060").is_none());
        assert!(ProxyAddr::parse("host:notaport").is_none());
    }

    #[test]
    fn resolves_ip_literal_to_socket_addr() {
        let a = ProxyAddr::new("127.0.0.1", 5060);
        assert_eq!(a.to_socket_addr().unwrap(), "127.0.0.1:5060".parse().unwrap());
    }

    #[test]
    fn from_socket_addr() {
        let s: SocketAddr = "192.168.1.1:5061".parse().unwrap();
        assert_eq!(ProxyAddr::from(s), ProxyAddr::new("192.168.1.1", 5061));
    }
}
