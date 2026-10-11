//! The URL grammar of an HTTP backend a runner names in its env:
//! `[http://]host:port[/]`, plain HTTP only. Another scheme, no port, a port
//! outside `1..=65535`, a path, a query or credentials, or an IPv6 host
//! written without brackets refuses boot.

use crate::stated;

/// The `host:port` the env value `key` states: `None` when unset or blank,
/// an `Err` naming `key` when malformed.
pub fn backend_url_from_lookup(
    key: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(url) = stated(lookup(key)) else {
        return Ok(None);
    };
    let refuse = |why: &str| format!("{key}={url:?}: {why}");
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => return Err(refuse(&format!("scheme {scheme} is not http"))),
        None => url.as_str(),
    };
    let hostport = rest.strip_suffix('/').unwrap_or(rest);
    if hostport.contains(['/', '?', '#', '@']) {
        return Err(refuse("only host:port, no path, query or credentials"));
    }
    let Some((host, port)) = hostport.rsplit_once(':') else {
        return Err(refuse("no port"));
    };
    if host.is_empty() {
        return Err(refuse("no host"));
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(refuse("an IPv6 host is written in brackets: [addr]:port"));
    }
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(refuse("the port is not in 1..=65535")),
        Ok(_) => Ok(Some(hostport.to_string())),
    }
}
