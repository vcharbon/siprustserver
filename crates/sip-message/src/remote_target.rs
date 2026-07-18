//! Remote-target header (`Contact`/`Route`/`Record-Route`) handling: the
//! ROUTING-CRITICAL part is host:port ONLY. Emission rewrites host:port to the
//! bound socket while preserving the user part and ALL parameters (URI and
//! header params, in their captured placement); matching compares the user part
//! and the two parameter sets (URI vs header) separately, host:port ignored.
//!
//! Structured parsing goes through the one SIP parser (`parse_name_addr` /
//! `parse_sip_uri_string`); only the host:port *rewrite* is targeted string
//! surgery, because reconstructing from the sorted parsed param maps would lose
//! the captured parameter order/placement the model must preserve.

use std::collections::BTreeMap;

use crate::message_helpers::split_top_level_commas;
use crate::parser::custom::compact_forms::expand_compact_form;
use crate::parser::custom::structured_headers::{parse_name_addr, parse_sip_uri_string};
use crate::types::ParamValue;

/// Canonical, case-folded header name (compact forms expanded).
pub(crate) fn canonical(name: &str) -> String {
    expand_compact_form(name).to_ascii_lowercase()
}

/// Header names whose routing-critical part is host:port only.
pub(crate) const REMOTE_TARGET: &[&str] = &["contact", "route", "record-route"];

/// Whether `name` (canonical or compact) is a remote-target header.
pub(crate) fn is_remote_target(name: &str) -> bool {
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

fn params_to_map(params: crate::types::Params) -> BTreeMap<String, String> {
    params
        .into_iter()
        .map(|(k, v)| {
            let val = match v {
                ParamValue::Flag => String::new(),
                ParamValue::Value(s) => s,
            };
            (k.to_ascii_lowercase(), val)
        })
        .collect()
}

fn parse_element(elem: &str) -> RtElement {
    let na = parse_name_addr(elem.trim());
    let (user, uri_params) = match parse_sip_uri_string(&na.uri) {
        Some(u) => (
            u.user,
            u.params.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect(),
        ),
        None => (None, BTreeMap::new()),
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
    let na = parse_name_addr(first.trim());
    let u = parse_sip_uri_string(&na.uri)?;
    Some((u.host, u.port.unwrap_or(5060) as u16))
}

/// Replace the host:port span of a single SIP-URI string, preserving scheme,
/// userinfo, and all URI parameters verbatim.
fn replace_uri_hostport(uri: &str, host: &str, port: u16) -> String {
    let scheme_end = match uri.find(':') {
        Some(i) => i + 1,
        None => return uri.to_string(),
    };
    let rest = &uri[scheme_end..];
    let params_at = rest.find([';', '?']).unwrap_or(rest.len());
    // Userinfo (before '@', before any parameter).
    let host_start_rel = match rest[..params_at].find('@') {
        Some(at) => at + 1,
        None => 0,
    };
    let host_start = scheme_end + host_start_rel;
    let after_host = &uri[host_start..];
    let host_end_rel = after_host.find([';', '?']).unwrap_or(after_host.len());
    let host_end = host_start + host_end_rel;
    format!("{}{}:{}{}", &uri[..host_start], host, port, &uri[host_end..])
}

/// Rewrite the host:port of every element in `value` to `(host, port)`,
/// preserving display name, user, URI/header params, and their placement.
fn rewrite_element_hostport(elem: &str, host: &str, port: u16) -> String {
    if let Some(lt) = elem.find('<') {
        if let Some(gt_rel) = elem[lt + 1..].find('>') {
            let gt = lt + 1 + gt_rel;
            let new_uri = replace_uri_hostport(&elem[lt + 1..gt], host, port);
            return format!("{}<{}>{}", &elem[..lt], new_uri, &elem[gt + 1..]);
        }
    }
    replace_uri_hostport(elem, host, port)
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
        assert_eq!(rewrite_hostport("<sip:proxy.example;lr>", "10.0.0.1", 5060), "<sip:10.0.0.1:5060;lr>");
        assert_eq!(rewrite_hostport("sip:a@h:5060", "1.2.3.4", 9), "sip:a@1.2.3.4:9");
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
