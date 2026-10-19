//! The gates the topmost Via alone meets: it names the hop this element
//! answers (RFC 3261 §18.2.2). A lower Via records an earlier hop, over any
//! transport §25.1 admits and with any port up to 65535 (ADR-0001 gates).

use super::extract_fields::is_valid_port;
use super::structured_headers::ParsedVia;
use crate::error::SipParseError;
use crate::parser::SipParserLimits;

/// RFC 3261 §8.1.1.7 — the top-Via branch starts with this magic cookie.
const VIA_BRANCH_MAGIC_COOKIE: &str = "z9hG4bK";

/// Whether a Via port is out of range: beyond 65535 on any Via, 0 on the
/// topmost one.
pub(super) fn port_out_of_range(port: u64, top: bool) -> bool {
    !is_valid_port(port) && (top || port != 0)
}

/// The refusal of a topmost Via whose branch lacks the magic cookie.
pub(super) fn cookie_refusal(v: &ParsedVia) -> Option<SipParseError> {
    match &v.branch {
        Some(b) if b.starts_with(VIA_BRANCH_MAGIC_COOKIE) => None,
        other => {
            let shown = match other {
                None => "<no branch>".to_string(),
                Some(b) => format!("\"{b}\""),
            };
            Some(SipParseError::new(format!(
                "Top Via branch missing magic cookie \"{VIA_BRANCH_MAGIC_COOKIE}\": {shown}"
            )))
        }
    }
}

/// The refusal of a topmost Via whose transport is not in the allowlist. The
/// allowlist is case-insensitive; the probe mints no uppercased String (the
/// set holds a handful of entries, linear is fine).
pub(super) fn transport_refusal(v: &ParsedVia, limits: &SipParserLimits) -> Option<SipParseError> {
    if limits.allowed_transports.iter().any(|t| t.eq_ignore_ascii_case(&v.transport)) {
        return None;
    }
    Some(SipParseError::new(format!("Via transport \"{}\" not in allowed set", v.transport)))
}
