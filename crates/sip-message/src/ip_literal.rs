//! The IP a SIP host names when it is an IP literal, and the socket address
//! of a `(host, port)` pair built from it, structurally. A SIP host holds an
//! IPv6 literal without its brackets (the parser strips them), so the text
//! `host:port` does not parse back for IPv6; the IP is read alone and joined
//! to the port instead. Nothing here resolves a name.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The IP an IP-literal host names: IPv4 dotted-quad or IPv6 ([`ipv6_address`]),
/// bracketed (`[::1]`) or not. `None` for a name or anything else.
pub fn ip_host(host: &str) -> Option<IpAddr> {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    bare.parse().ok().or_else(|| extra_colon_ipv6(bare).map(IpAddr::V6))
}

/// The IPv6 address `text` names: the RFC 4291 §2.2 text form (RFC 5954
/// §4.1), or the one form beyond it RFC 5118 §4.10 has a parser tolerate,
/// three colons directly before a dotted-quad IPv4 tail, read as `::`
/// (`2001:db8:::192.0.2.1` names `2001:db8::192.0.2.1`). No zone ID.
pub fn ipv6_address(text: &str) -> Option<Ipv6Addr> {
    text.parse().ok().or_else(|| extra_colon_ipv6(text))
}

/// The RFC 5118 §4.10 extra-colon form read as the `::` form, or `None`.
fn extra_colon_ipv6(text: &str) -> Option<Ipv6Addr> {
    let (head, tail) = text.rsplit_once(':')?;
    if !head.ends_with("::") || head.ends_with(":::") || tail.parse::<Ipv4Addr>().is_err() {
        return None;
    }
    format!("{head}{tail}").parse().ok()
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

    /// RFC 5118 §4.10: `:::` before an IPv4 tail reads as `::`, and only there.
    #[test]
    fn the_extra_colon_form_names_the_same_address() {
        let same = ipv6_address("2001:db8::192.0.2.1");
        assert!(same.is_some());
        assert_eq!(ipv6_address("2001:db8:::192.0.2.1"), same);
        assert_eq!(ip_host("[2001:db8:::192.0.2.1]"), same.map(IpAddr::V6));
        assert_eq!(ipv6_address(":::192.0.2.1"), ipv6_address("::192.0.2.1"));
        for refused in ["1:2:::3", "2001:db8::::192.0.2.1", "2001:db8:::192.0.2", "1:2:::3::4"] {
            assert_eq!(ipv6_address(refused), None, "{refused}");
            assert_eq!(ip_host(refused), None, "{refused}");
        }
    }
}
