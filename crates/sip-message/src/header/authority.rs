//! The host of an absolute URI that names a network authority (RFC 3986
//! §3.2: `scheme://[userinfo@]host[:port][/path…]`), the form `Call-Info`,
//! `Alert-Info` and `Error-Info` carry an `http:` or `https:` reference in.
//! A SIP, tel or URN URI has no `//` authority and names no host here. A
//! message id (`cid:`, `mid:`, `Content-ID`) names its domain after its `@`.

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
    let authority = rest.split(['/', '?', '#', '>']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host_port.starts_with('[') {
        host_port.get(..=host_port.find(']')?)?
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then_some(host)
}

/// The host of a `hostport` (RFC 3261 §25.1: `host [":" port]`), an IPv6
/// reference with its brackets: the port, which names no one, left out.
pub fn host_of(hostport: &str) -> &str {
    let hostport = hostport.trim();
    if hostport.starts_with('[') {
        return hostport.split_inclusive(']').next().unwrap_or(hostport);
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => hostport,
    }
}

/// The domain of a message id: a `cid:` or `mid:` URI (RFC 2392) or a
/// `Content-ID` value (`<id-left@id-right>`, RFC 5322 §3.6.4), the text
/// after its last `@`; `None` for any other value.
pub fn message_id_domain(value: &str) -> Option<&str> {
    let value = value.trim().trim_start_matches('<').trim_end_matches('>');
    let id = ["cid:", "mid:"].iter().find_map(|scheme| {
        value
            .get(..scheme.len())
            .filter(|s| s.eq_ignore_ascii_case(scheme))
            .and_then(|_| value.get(scheme.len()..))
    });
    // With no message-id scheme, a value naming one has no scheme at all.
    let id = id.or_else(|| (!value.contains(':')).then_some(value))?;
    let (_, domain) = id.rsplit_once('@')?;
    let domain = domain.split(['/', '>', ';']).next().unwrap_or_default();
    (!domain.is_empty() && !domain.contains(':')).then_some(domain)
}

#[cfg(test)]
mod tests {
    use super::{host_of, message_id_domain, network_host};

    #[test]
    fn a_hostport_names_its_host_without_the_port() {
        assert_eq!(host_of("sbc.example.com:5070"), "sbc.example.com");
        assert_eq!(host_of("192.0.2.1:5080"), "192.0.2.1");
        assert_eq!(host_of("[2001:db8::1]:5060"), "[2001:db8::1]");
        assert_eq!(host_of("2001:db8::1"), "2001:db8::1");
        assert_eq!(host_of("pseudonym"), "pseudonym");
    }

    #[test]
    fn a_message_id_names_its_domain() {
        assert_eq!(message_id_domain("<cid:part1@host.example.com>"), Some("host.example.com"));
        assert_eq!(message_id_domain("<4ab1@example.org>"), Some("example.org"));
        assert_eq!(message_id_domain("sip:alice@example.com"), None);
        assert_eq!(message_id_domain("no-at-sign"), None);
    }

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
