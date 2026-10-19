//! The IP a SIP host names when it is an IP literal, and the socket address
//! of a `(host, port)` pair built from it, structurally. A SIP host holds an
//! IPv6 literal without its brackets (the parser strips them), so the text
//! `host:port` does not parse back for IPv6; the IP is read alone and joined
//! to the port instead. Nothing here resolves a name.

use std::net::{IpAddr, SocketAddr};

/// The IP an IP-literal host names: IPv4 dotted-quad or IPv6, bracketed
/// (`[::1]`) or not. `None` for a name or anything else.
pub fn ip_host(host: &str) -> Option<IpAddr> {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    bare.parse().ok()
}

/// The socket address of an IP-literal `host` at `port`. `None` when `host`
/// is not an IP literal.
pub fn ip_socket_addr(host: &str, port: u16) -> Option<SocketAddr> {
    ip_host(host).map(|ip| SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipv4_literal_joins_its_port() {
        assert_eq!(ip_socket_addr("10.0.0.9", 5060), Some("10.0.0.9:5060".parse().unwrap()));
    }

    #[test]
    fn a_bare_ipv6_literal_joins_its_port() {
        assert_eq!(
            ip_socket_addr("2001:db8::1", 5070),
            Some("[2001:db8::1]:5070".parse().unwrap())
        );
        assert_eq!(ip_socket_addr("::1", 5060), Some("[::1]:5060".parse().unwrap()));
    }

    #[test]
    fn a_bracketed_ipv6_literal_joins_its_port() {
        assert_eq!(
            ip_socket_addr("[2001:db8::1]", 5070),
            Some("[2001:db8::1]:5070".parse().unwrap())
        );
    }

    #[test]
    fn a_name_is_no_socket_address() {
        assert_eq!(ip_socket_addr("callee.ns.svc.cluster.local", 5060), None);
        assert_eq!(ip_socket_addr("localhost", 5060), None);
        assert_eq!(ip_socket_addr("", 5060), None);
        assert_eq!(ip_socket_addr("[broken", 5060), None);
        assert_eq!(ip_socket_addr("999.1.1.1", 5060), None);
    }
}
