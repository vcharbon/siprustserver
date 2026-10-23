//! Remote-target header (`Contact`/`Route`/`Record-Route`) handling: the
//! ROUTING-CRITICAL part is host:port ONLY. Emission rewrites host:port to the
//! bound socket while preserving the user part and ALL parameters (URI and
//! header params, in their captured placement); matching compares the user part
//! and the two parameter sets (URI vs header) separately, host:port ignored.
//!
//! Structured parsing goes through the parser's name-addr scan and the one URI
//! reader ([`Uri::parse`]). The host:port *rewrite* reads each element with the
//! strict name-addr reader and splices the re-rendered URI into the element,
//! so the display name and header parameters keep their captured spelling and
//! placement.

use std::collections::BTreeMap;

use crate::header::{AddrForm, HostPort, NameAddr, Uri, UriRole};
use crate::parser::custom::compact_forms::expanded_name;
use crate::parser::custom::structured_headers::{parse_name_addr, split_top_level_commas};
use crate::sip_str::SipStr;

/// Canonical, case-folded header name (compact forms expanded).
pub fn canonical(name: &str) -> String {
    expanded_name(name).to_ascii_lowercase()
}

/// Header names whose routing-critical part is host:port only.
pub(crate) const REMOTE_TARGET: &[&str] = &["contact", "route", "record-route"];

/// Whether `name` (canonical or compact) is a remote-target header.
pub fn is_remote_target(name: &str) -> bool {
    REMOTE_TARGET.contains(&canonical(name).as_str())
}

/// One parsed element of a remote-target header value (a header may carry
/// several comma-separated elements — a `Route` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtElement {
    pub user: Option<String>,
    /// URI parameters (inside the addr-spec), lowercase keys, flag = empty value.
    pub uri_params: BTreeMap<String, String>,
    /// Header (name-addr) parameters (after `>`), lowercase keys, flag = empty.
    pub header_params: BTreeMap<String, String>,
}

fn params_to_map(params: crate::header::Params) -> BTreeMap<String, String> {
    params
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or("").to_string()))
        .collect()
}

fn parse_element(elem: &str) -> RtElement {
    let na = parse_name_addr(&SipStr::owned(elem.trim()));
    let (user, uri_params) = match Uri::parse(&na.uri) {
        Ok(u) => (u.user().map(String::from), params_to_map(u.params().clone())),
        Err(_) => (None, BTreeMap::new()),
    };
    RtElement { user, uri_params, header_params: params_to_map(na.params) }
}

/// Parse the comma-separated elements of a remote-target header value.
pub fn parse_elements(value: &str) -> Vec<RtElement> {
    split_top_level_commas(value).into_iter().map(parse_element).collect()
}

/// host:port of the FIRST element — reads the stack's bound socket off a
/// generated header so the captured value can be rewritten onto it.
pub fn first_hostport(value: &str) -> Option<(String, u16)> {
    let first = split_top_level_commas(value).into_iter().next()?;
    let na = parse_name_addr(&SipStr::owned(first.trim()));
    let u = Uri::parse(&na.uri).ok()?;
    let (host, port) = u.host_port();
    Some((host.to_string(), port))
}

/// Rewrite the host:port of one element to `(host, port)`. The element is read
/// by the strict name-addr reader, so the URI rewritten is the one its grammar
/// names, never a `<` inside a quoted display name; the URI is re-rendered
/// from its parts and spliced back, its scheme and every byte around it kept
/// as written. An element the reader refuses, or whose URI is not `sip` /
/// `sips`, comes back as it is.
fn rewrite_element_hostport(elem: &str, host: &str, port: u16) -> String {
    rewritten_element(elem, host, port).unwrap_or_else(|| elem.to_string())
}

fn rewritten_element(elem: &str, host: &str, port: u16) -> Option<String> {
    let value = SipStr::owned(elem);
    let (addr, span) =
        NameAddr::parse_spanned(&value, AddrForm::NameAddrOrSpec, UriRole::Target).ok()?;
    let scheme = addr.uri().scheme();
    if !matches!(scheme, "sip" | "sips") {
        return None;
    }
    let uri = addr.uri().clone().with_authority(HostPort::new(host, Some(port))).to_string();
    Some(format!(
        "{}{}{}{}",
        elem.get(..span.start)?,
        elem.get(span.start..span.start + scheme.len())?,
        uri.get(scheme.len()..)?,
        elem.get(span.end..)?
    ))
}

/// Rewrite host:port across every comma-separated element of a remote-target
/// header value.
pub fn rewrite_hostport(value: &str, host: &str, port: u16) -> String {
    split_top_level_commas(value)
        .into_iter()
        .map(|e| rewrite_element_hostport(e.trim(), host, port))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port past 16 bits names no socket: no host:port, rather than the
    /// port's low 16 bits.
    #[test]
    fn a_port_past_16_bits_reads_as_none() {
        assert_eq!(first_hostport("<sip:a@192.0.2.1:70000>"), None);
        assert_eq!(first_hostport("<sip:a@192.0.2.1:5070>"), Some(("192.0.2.1".into(), 5070)));
    }

    #[test]
    fn rewrites_hostport_preserving_user_and_params() {
        let got = rewrite_hostport(
            "<sip:+48123@203.0.113.7:5060;user=phone>;expires=3600",
            "127.0.0.1",
            5070,
        );
        assert_eq!(got, "<sip:+48123@127.0.0.1:5070;user=phone>;expires=3600");
    }

    #[test]
    fn rewrites_bare_and_userless_uris() {
        assert_eq!(
            rewrite_hostport("<sip:proxy.example;lr>", "10.0.0.1", 5060),
            "<sip:10.0.0.1:5060;lr>"
        );
        assert_eq!(rewrite_hostport("sip:a@h:5060", "1.2.3.4", 9), "sip:a@1.2.3.4:9");
    }

    /// A `<` inside a quoted display name opens no URI: the address the
    /// name-addr grammar reads is the one rewritten.
    #[test]
    fn a_uri_quoted_in_the_display_name_is_left_as_written() {
        assert_eq!(
            rewrite_hostport(
                "\"Alice <sip:evil@example.com>\" <sip:real@example.com>;expires=60",
                "192.0.2.1",
                5070
            ),
            "\"Alice <sip:evil@example.com>\" <sip:real@192.0.2.1:5070>;expires=60"
        );
    }

    /// The userinfo ends at the `@`, even where it carries a `;`.
    #[test]
    fn a_semicolon_in_the_user_stays_in_the_user() {
        assert_eq!(
            rewrite_hostport("<sip:+331;npdi@example.com:5060;user=phone>", "192.0.2.1", 5070),
            "<sip:+331;npdi@192.0.2.1:5070;user=phone>"
        );
    }

    #[test]
    fn multi_value_route_rewrites_each_element() {
        let got = rewrite_hostport("<sip:p1;lr>, <sip:p2;lr;primary=x>", "9.9.9.9", 5060);
        assert_eq!(got, "<sip:9.9.9.9:5060;lr>, <sip:9.9.9.9:5060;lr;primary=x>");
    }

    #[test]
    fn parses_uri_and_header_params_separately() {
        let els = parse_elements("<sip:bob@h:5060;transport=udp>;expires=60;q=0.5");
        assert_eq!(els.len(), 1);
        assert_eq!(els[0].user.as_deref(), Some("bob"));
        assert_eq!(els[0].uri_params.get("transport"), Some(&"udp".to_string()));
        assert_eq!(els[0].header_params.get("expires"), Some(&"60".to_string()));
        assert_eq!(els[0].header_params.get("q"), Some(&"0.5".to_string()));
        // A URI param and a header param never merge.
        assert!(!els[0].uri_params.contains_key("expires"));
        assert!(!els[0].header_params.contains_key("transport"));
    }

    #[test]
    fn multi_value_route_parses_per_element() {
        let els = parse_elements("<sip:p1;lr>,<sip:p2;lr;primary=x>");
        assert_eq!(els.len(), 2);
        assert!(els[0].uri_params.contains_key("lr"));
        assert_eq!(els[1].uri_params.get("primary"), Some(&"x".to_string()));
        // Element 2's params did not leak onto element 1.
        assert!(!els[0].uri_params.contains_key("primary"));
    }
}
