//! The host of an absolute URI that names a network authority (RFC 3986
//! §3.2: `scheme://[userinfo@]host[:port][/path…]`), the form `Call-Info`,
//! `Alert-Info` and `Error-Info` carry an `http:` or `https:` reference in.
//! A SIP, tel or URN URI has no `//` authority and names no host here.

/// The host of `uri`'s authority — an IPv6 literal with its brackets — or
/// `None` where the URI has no `//` authority or an empty host.
pub fn network_host(uri: &str) -> Option<&str> {
    let uri = uri.trim().trim_start_matches('<');
    let (scheme, rest) = uri.split_once("://")?;
    if scheme.is_empty()
        || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    {
        return None;
    }
    let authority = &rest[..rest.find(['/', '?', '#', '>']).unwrap_or(rest.len())];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host_port.starts_with('[') {
        &host_port[..=host_port.find(']')?]
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::network_host;

    #[test]
    fn the_host_of_an_authority_is_read_and_nothing_else_names_one() {
        assert_eq!(network_host("<http://media.example.com/ring.wav>"), Some("media.example.com"));
        assert_eq!(network_host("https://user@www.example.org:8443/a?b"), Some("www.example.org"));
        assert_eq!(network_host("http://[2001:db8::1]:80/x"), Some("[2001:db8::1]"));
        assert_eq!(network_host("sip:alice@example.com"), None);
        assert_eq!(network_host("tel:+15550100"), None);
        assert_eq!(network_host("http:///path"), None);
    }
}
