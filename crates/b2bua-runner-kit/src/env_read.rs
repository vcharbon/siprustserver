//! How a knob's text reads: trimmed, blank stating nothing, a `host:port` split.

use std::env;

use crate::split_csv;

/// `value` trimmed, `None` when it is absent or blank: a knob set to nothing
/// states nothing.
pub fn stated(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The comma-separated names `value` lists, `None` when it names none: a list
/// of blanks states nothing, as a blank value does.
pub fn stated_list(value: Option<String>) -> Option<Vec<String>> {
    stated(value).map(|v| split_csv(&v)).filter(|names| !names.is_empty())
}

/// The env var `key` as [`stated`] reads it.
pub fn env_stated(key: &str) -> Option<String> {
    stated(env::var(key).ok())
}

/// Split a `host:port` into its parts WITHOUT DNS resolution (the host may be a
/// service name resolved per-call downstream). Port defaults to 5060 if absent
/// or unparseable. Used for a b-leg callee default (`B2BUA_DEST`-style knobs) —
/// resolving at boot would pin every call to one startup-resolved pod.
pub fn split_host_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(5060)),
        None => (s.to_string(), 5060),
    }
}
