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

/// Canonical USER identity of a `sip:`/`sips:`/`tel:` URI for cross-entity
/// comparison: the userinfo of a sip URI (user-part `;`-params such as
/// `verstat`/`phone-context` dropped) or the subscriber part of a tel URI.
/// A phone-shaped identity (optional `+`, then digits possibly interleaved
/// with RFC 3966 visual separators `-`, `.`, `(`, `)`) normalizes by
/// dropping the separators, so `tel:+1-408-555-1212` and
/// `sip:+14085551212@host` yield the same identity. Scheme, host, port and
/// URI params never participate. `None` for a userless sip URI or an
/// unparsable value.
pub fn uri_user_identity(uri: &str) -> Option<String> {
    let p = parse_sip_uri(uri)?;
    let raw = match p.scheme.as_str() {
        // tel: has no userinfo — the subscriber number sits in the host slot
        // (tel params were already split off into `params`).
        "tel" => p.host,
        _ => p.user?,
    };
    // Userinfo params (`;verstat=…`, `;phone-context=…`) are not identity.
    let user = raw.split(';').next().unwrap_or("");
    if user.is_empty() {
        return None;
    }
    let stripped: String = user.chars().filter(|c| !matches!(c, '-' | '.' | '(' | ')')).collect();
    let phone_shaped = {
        let digits = stripped.strip_prefix('+').unwrap_or(&stripped);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    Some(if phone_shaped { stripped } else { user.to_string() })
}

/// Whether two URIs (any mix of `sip:`/`sips:`/`tel:` forms) name the same
/// user identity — [`uri_user_identity`] on both sides, compared BYTE-EXACT:
/// the user part is case-sensitive (RFC 3261 §19.1.4), so `Alice` != `alice`.
/// Phone-shaped identities are already normalized (scheme dropped, visual
/// separators stripped, digits only), so `TEL:+333` ≡ `tel:+333` and
/// case-insensitivity would be vacuous for them. `false` when either side
/// has no identity.
pub fn same_user_identity(a: &str, b: &str) -> bool {
    match (uri_user_identity(a), uri_user_identity(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}
