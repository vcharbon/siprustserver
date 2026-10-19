//! The checks a URI passes to stand as a Request-URI, the strict wire grammar
//! aside: every request is held to them at its freeze, so a URI that fails one
//! is no request's target, whether a peer sent it in a start line or recorded
//! it as a route (RFC 3261 §12.2.1.1, §16.12).
//!
//! The byte scans test ASCII structural bytes only, so a UTF-8 lead or
//! continuation byte can never alias one.

use super::extract_fields::is_valid_port;
use super::port_garbage::has_uri_port_trailing_garbage;
use super::structured_headers::parse_sip_uri_string;
use crate::error::SipParseError;
use crate::sip_str::SipStr;

/// Why `uri` cannot stand as a Request-URI, if it cannot.
pub(crate) fn request_uri_refusal(uri: &SipStr) -> Result<(), SipParseError> {
    if has_unescaped_ctl_bytes(uri) {
        return Err(SipParseError::new(format!("Control byte in Request-URI: \"{uri}\"")));
    }
    if has_unbalanced_square_brackets(uri) {
        return Err(SipParseError::new(format!(
            "Unbalanced IPv6 brackets in Request-URI: \"{uri}\""
        )));
    }
    if has_uri_port_trailing_garbage(uri) {
        return Err(SipParseError::new(format!(
            "Trailing non-digit after Request-URI port: \"{uri}\""
        )));
    }
    if has_unbracketed_ipv6(uri) {
        return Err(SipParseError::new(format!("Unbracketed IPv6 in Request-URI: \"{uri}\"")));
    }
    let parsed = parse_sip_uri_string(uri)
        .ok_or_else(|| SipParseError::new(format!("Malformed Request-URI: \"{uri}\"")))?;
    if parsed.host.is_empty() {
        return Err(SipParseError::new(format!("Empty host in Request-URI: \"{uri}\"")));
    }
    match parsed.port {
        Some(p) if !is_valid_port(p) => {
            Err(SipParseError::new(format!("Request-URI port out of range: {p}")))
        }
        _ => Ok(()),
    }
}

/// True iff the URI carries unescaped control bytes (0x00-0x1F except HTAB,
/// or 0x7F).
fn has_unescaped_ctl_bytes(uri: &str) -> bool {
    uri.bytes().any(|b| b != 0x09 && (b < 0x20 || b == 0x7f))
}

/// True iff `uri` contains `[` without a matching `]`.
fn has_unbalanced_square_brackets(uri: &str) -> bool {
    let mut open: i32 = 0;
    for c in uri.bytes() {
        if c == b'[' {
            open += 1;
        } else if c == b']' {
            if open == 0 {
                return true;
            }
            open -= 1;
        }
    }
    open != 0
}

/// True iff the host portion has 2+ colons outside `[...]` — IPv6 without the
/// required bracket delimiters (RFC 5118 §4.2).
fn has_unbracketed_ipv6(uri: &str) -> bool {
    let s = uri.as_bytes();
    let scheme_colon = match s.iter().position(|&c| c == b':') {
        Some(i) => i,
        None => return false,
    };
    let after_scheme = scheme_colon + 1;
    let host_start = s
        .iter()
        .skip(after_scheme)
        .position(|&c| c == b'@')
        .map_or(after_scheme, |at| after_scheme + at + 1);
    let mut depth = 0i32;
    let mut colons = 0i32;
    for &c in s.iter().skip(host_start) {
        if c == b'[' {
            depth += 1;
        } else if c == b']' && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            if c == b':' {
                colons += 1;
                if colons >= 2 {
                    return true;
                }
            } else if c == b';' || c == b'?' || c == b'>' {
                break;
            }
        }
    }
    false
}
