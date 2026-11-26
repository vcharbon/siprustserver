//! [`ProxyAddr`], the proxy's `(host, port)` target. The host stays a string,
//! an IP literal or a name, until the send: [`ProxyAddr::to_socket_addr`]
//! reads a literal, and the named forwarder resolves a name.

use std::net::SocketAddr;

use sip_message::header::HostPort;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyAddr {
    pub host: String,
    pub port: u16,
}

impl ProxyAddr {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self { host: host.into(), port }
    }

    /// Parses `host:port` (the cookie, registry and configuration form) as an
    /// RFC 3261 `hostport` with its port ([`HostPort::parse`]): `host` holds
    /// the bare address, an IPv6 literal written only bracketed
    /// (`[2001:db8::7]:5060`). `None` for a host the URI grammar refuses
    /// (empty, malformed, an unbracketed IPv6 address), no port, a port of 0
    /// or past 65535, or any byte after the port.
    pub fn parse(s: &str) -> Option<Self> {
        let read = HostPort::parse(s).ok()?;
        Some(Self { host: read.host().to_string(), port: read.port()? })
    }

    /// The socket address of an IP-literal host (IPv6 bracketed or not); `None`
    /// for a name, which the named forwarder resolves.
    pub fn to_socket_addr(&self) -> Option<SocketAddr> {
        sip_net::ip_socket_addr(&self.host, self.port)
    }
}

/// `host:port`, an IPv6 host bracketed: the form [`ProxyAddr::parse`] reads
/// back.
impl std::fmt::Display for ProxyAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
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

    /// An IPv6 literal is read only bracketed and held bare; `Display`
    /// brackets it again, so the text round-trips.
    #[test]
    fn an_ipv6_host_is_read_bracketed_and_held_bare() {
        let a = ProxyAddr::parse("[2001:db8::7]:5060").unwrap();
        assert_eq!(a, ProxyAddr::new("2001:db8::7", 5060));
        assert_eq!(a.to_socket_addr(), Some("[2001:db8::7]:5060".parse().unwrap()));
        assert_eq!(a.to_string(), "[2001:db8::7]:5060");
        assert_eq!(ProxyAddr::parse(&a.to_string()), Some(a));
        assert!(ProxyAddr::parse("2001:db8::7:5060").is_none(), "unbracketed: no port to tell");
        assert!(ProxyAddr::parse("[2001:db8::7]").is_none(), "no port");
    }

    #[test]
    fn a_port_is_from_1_to_65535() {
        assert_eq!(ProxyAddr::parse("host.example:1"), Some(ProxyAddr::new("host.example", 1)));
        assert!(ProxyAddr::parse("host.example:65535").is_some());
        assert!(ProxyAddr::parse("host.example:0").is_none());
        assert!(ProxyAddr::parse("host.example:65536").is_none());
    }

    #[test]
    fn resolves_ip_literal_to_socket_addr() {
        let a = ProxyAddr::new("127.0.0.1", 5060);
        assert_eq!(a.to_socket_addr().unwrap(), "127.0.0.1:5060".parse().unwrap());
    }

    #[test]
    fn an_ipv6_peer_resolves_back_to_its_socket_addr() {
        let s: SocketAddr = "[2001:db8::7]:5060".parse().unwrap();
        assert_eq!(ProxyAddr::from(s).to_socket_addr(), Some(s));
    }

    #[test]
    fn a_name_is_left_to_the_named_forwarder() {
        assert_eq!(ProxyAddr::new("callee.example.test", 5060).to_socket_addr(), None);
    }

    #[test]
    fn from_socket_addr() {
        let s: SocketAddr = "192.168.1.1:5061".parse().unwrap();
        assert_eq!(ProxyAddr::from(s), ProxyAddr::new("192.168.1.1", 5061));
    }
}
