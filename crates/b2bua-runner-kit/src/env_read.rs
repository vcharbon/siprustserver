//! How a knob's text reads: trimmed, and blank states nothing.

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
