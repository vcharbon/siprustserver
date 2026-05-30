//! MessageHelpers — SIP header accessors + structured-header readers. Port of
//! the **pure** half of `src/sip/MessageHelpers.ts`: it reads and rewrites
//! existing headers rather than constructing new ones (construction lives in
//! [`crate::generators`]).
//!
//! Deferred to slice 2 (network/dispatch), NOT ported here:
//!   - the identifier generators (`newTag`/`newBranch`/`newCallId`/`currentRng`)
//!     — they read a fiber-local seeded RNG (Effect `Random`); the Rust port
//!     will inject an RNG seam at the network layer.
//!   - the byte-level overload/dispatcher helpers
//!     (`buildStatelessReject503Buffer`, `isInviteRequestBuffer`,
//!     `bufferHasEmergencyMarker`, `bufferHasToTag`, `jitteredRetryAfter`) —
//!     Tier-1 UDP / dispatcher concerns.

use std::collections::BTreeMap;

use crate::parser::custom::structured_headers::{
    parse_contact, parse_name_addr, parse_sip_uri_string, parse_via,
};
use crate::types::{ParamValue, SipHeader, SipRequest};

/// First header value matching `name` (case-insensitive).
pub fn get_header<'a>(headers: &'a [SipHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// All header values matching `name` (case-insensitive), in wire order.
pub fn get_headers<'a>(headers: &'a [SipHeader], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
        .collect()
}

/// Set or replace a header (first occurrence). Returns a new header list.
pub fn set_header(headers: &[SipHeader], name: &str, value: &str) -> Vec<SipHeader> {
    let mut result = headers.to_vec();
    match result.iter_mut().find(|h| h.name.eq_ignore_ascii_case(name)) {
        Some(h) => {
            h.name = name.to_string();
            h.value = value.to_string();
        }
        None => result.push(SipHeader { name: name.to_string(), value: value.to_string() }),
    }
    result
}

/// Remove all headers matching `name` (case-insensitive).
pub fn remove_header(headers: &[SipHeader], name: &str) -> Vec<SipHeader> {
    headers.iter().filter(|h| !h.name.eq_ignore_ascii_case(name)).cloned().collect()
}

/// Extract the `tag` parameter from a From/To header value. Quote-aware.
pub fn extract_tag(header_value: &str) -> Option<String> {
    parse_name_addr(header_value).tag
}

/// Strip the `tag` parameter from a From/To header value, reconstructing the
/// remaining name-addr + params. Quote-aware.
pub fn strip_tag(header_value: &str) -> String {
    let parsed = parse_name_addr(header_value);
    if parsed.tag.is_none() {
        return header_value.to_string();
    }

    let mut result = String::new();
    if let Some(dn) = &parsed.display_name {
        result.push_str(&format!("\"{dn}\" "));
    }
    result.push_str(&format!("<{}>", parsed.uri));
    for (k, v) in &parsed.params {
        if k == "tag" {
            continue;
        }
        match v {
            ParamValue::Flag => result.push_str(&format!(";{k}")),
            ParamValue::Value(val) => result.push_str(&format!(";{k}={val}")),
        }
    }
    result
}

/// Extract the URI from a From/To header value (name-addr). Quote-aware.
pub fn extract_name_addr_uri(header_value: &str) -> String {
    parse_name_addr(header_value).uri
}

/// Extract the URI from a Contact header value. Quote-aware.
pub fn extract_contact_uri(contact_value: &str) -> String {
    parse_contact(contact_value).uri
}

/// Parsed SIP URI fields (port default 5060).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSipUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: u64,
    pub params: BTreeMap<String, String>,
}

/// Parse a SIP URI (angle brackets stripped if present). Zero-regex.
pub fn parse_sip_uri(uri: &str) -> Option<ParsedSipUri> {
    let mut cleaned = uri;
    if let Some(lt) = uri.find('<') {
        cleaned = match uri[lt + 1..].find('>') {
            Some(rel) => &uri[lt + 1..lt + 1 + rel],
            None => &uri[lt + 1..],
        };
    }
    let parsed = parse_sip_uri_string(cleaned)?;
    Some(ParsedSipUri {
        scheme: parsed.scheme,
        user: parsed.user,
        host: parsed.host,
        port: parsed.port.unwrap_or(5060),
        params: parsed.params,
    })
}

/// Extract host:port from a SIP URI string.
pub fn extract_host_port(uri: &str) -> Option<(String, u64)> {
    let parsed = parse_sip_uri(uri)?;
    Some((parsed.host, parsed.port))
}

/// Parse URI parameters from a SIP URI (e.g. `sip:b2bua@host;callRef=abc;leg=a`).
pub fn parse_uri_params(uri: &str) -> BTreeMap<String, String> {
    parse_sip_uri(uri).map(|p| p.params).unwrap_or_default()
}

/// The B2BUA's custom Via parameters: `branch`, `cr`, `lg`. Zero-regex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViaParams {
    pub branch: Option<String>,
    pub cr: Option<String>,
    pub lg: Option<String>,
}

pub fn parse_via_params(via_value: &str) -> ViaParams {
    let parsed = parse_via(via_value);
    let pick = |key: &str| match parsed.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };
    ViaParams { branch: parsed.branch, cr: pick("cr"), lg: pick("lg") }
}

/// Whether a request carries an emergency Resource-Priority header
/// (esnet.0 / wps.0 / q735.0). Case-insensitive name, case-sensitive value.
pub fn is_emergency_request(req: &SipRequest) -> bool {
    match get_header(&req.headers, "resource-priority") {
        None => false,
        Some(value) => {
            value.contains("esnet.0") || value.contains("wps.0") || value.contains("q735.0")
        }
    }
}
