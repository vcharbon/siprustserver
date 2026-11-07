//! The gates the topmost Via alone meets: it names the hop this element
//! answers (RFC 3261 §18.2.2). A lower Via records an earlier hop, over any
//! transport §25.1 admits and with any port up to 65535 (ADR-0001 gates).

use super::extract_fields::is_valid_port;
use crate::error::SipParseError;
use crate::header::{has_magic_cookie, Via, BRANCH_MAGIC_COOKIE};
use crate::parser::SipParserLimits;

/// Whether a Via port is out of range: beyond 65535 on any Via, 0 on the
/// topmost one.
pub(super) fn port_out_of_range(port: u16, top: bool) -> bool {
    !is_valid_port(u64::from(port)) && (top || port != 0)
}

/// The refusal of a topmost Via whose branch lacks the magic cookie, or is
/// the cookie alone: a branch is the cookie followed by the transaction's
/// identifier (RFC 3261 §8.1.1.7; RFC 4475 §3.2.1). The cookie compares
/// ignoring ASCII case, as the whole branch does (§7.3.1).
pub(super) fn cookie_refusal(v: &Via) -> Option<SipParseError> {
    let shown = match v.branch() {
        Some(b) if b.len() > BRANCH_MAGIC_COOKIE.len() && has_magic_cookie(b) => return None,
        Some(b) if has_magic_cookie(b) => {
            return Some(SipParseError::new(
                "Top Via branch is the magic cookie alone, no transaction identifier",
            ))
        }
        None => "<no branch>".to_string(),
        Some(b) => format!("{b:?}"),
    };
    Some(SipParseError::new(format!(
        "Top Via branch missing magic cookie {BRANCH_MAGIC_COOKIE}: {shown}"
    )))
}

/// The refusal of a topmost Via whose transport is not in the allowlist. The
/// allowlist is case-insensitive; the probe mints no uppercased String (the
/// set holds a handful of entries, linear is fine).
pub(super) fn transport_refusal(v: &Via, limits: &SipParserLimits) -> Option<SipParseError> {
    if limits.allowed_transports.iter().any(|t| t.eq_ignore_ascii_case(v.transport())) {
        return None;
    }
    Some(SipParseError::new(format!("Via transport {:?} not in allowed set", v.transport())))
}
