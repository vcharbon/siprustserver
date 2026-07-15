//! SIP-URI string parsing: [`ParsedSipUri`] plus host:port and
//! URI-parameter extraction. Surrounding angle brackets are stripped; the
//! port defaults to 5060. Zero-regex.

use std::collections::BTreeMap;

use crate::parser::custom::structured_headers::parse_sip_uri_string;

/// Parsed SIP URI fields (port default 5060).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSipUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: u64,
    pub params: BTreeMap<String, String>,
}

/// Parse a SIP URI (angle brackets stripped if present).
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
