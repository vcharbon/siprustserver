//! The `ip:port` grammar of an HTTP listener's address variable.

use std::net::{Ipv6Addr, SocketAddr};

/// `value` as a socket address: `192.0.2.1:9090`, `[2001:db8::1]:9090`, or an
/// IPv6 address followed by `:port` without brackets (`2001:db8::1:9090`),
/// the form an orchestrator's `$(VAR):port` expansion of an IPv6 address
/// gives. The port is always the last `:` field. `None` for anything else,
/// a host name included.
pub fn parse_socket_addr(value: &str) -> Option<SocketAddr> {
    let value = value.trim();
    if let Ok(addr) = value.parse() {
        return Some(addr);
    }
    let (ip, port) = value.rsplit_once(':')?;
    Some(SocketAddr::new(ip.parse::<Ipv6Addr>().ok()?.into(), port.parse().ok()?))
}
