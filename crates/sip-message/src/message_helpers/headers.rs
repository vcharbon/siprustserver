//! Accessors over a parsed header list (`&[SipHeader]`): lookup, set,
//! remove. Header names match case-insensitively (RFC 3261 §7.3.1); values
//! are returned verbatim.

use crate::types::SipHeader;

/// Canonical top-level comma splitter (quote-, escape- AND angle-bracket-
/// aware) — the ONE implementation for header folding everywhere; never
/// re-implement it in another crate.
pub use crate::parser::custom::structured_headers::split_top_level_commas;

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
