//! Named-parameter extraction from a single header VALUE — `;`-separated
//! `name[=value]` items, quote-aware, name match ASCII-case-insensitive.
//! Serves headers whose whole value is a parameter list (the IMS
//! P-Charging-Vector `icid-value=…;icid-generated-at=…` family) as well as
//! values with a leading non-param segment (which is skipped).

use crate::header::Params;
use crate::sip_str::SipStr;

/// Value of the named `;`-parameter within one header VALUE (not a header
/// list). Quoted-string values are unquoted (backslash escapes resolved);
/// a valueless (flag) parameter yields `Some("")`; `None` when absent.
///
/// Intended for parameter-list-shaped values (P-Charging-Vector and
/// friends); a free-form leading segment containing an unquoted `;` is not
/// re-interpreted, so From/Contact display names are out of scope — use the
/// name-addr readers for those.
pub fn header_param_value(value: &str, name: &str) -> Option<String> {
    let params = Params::parse_list(&SipStr::owned(value));
    Some(params.get(name)?.as_str().unwrap_or("").to_string())
}
