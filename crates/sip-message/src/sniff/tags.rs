//! The `tag` parameters of a raw datagram's To and From headers (RFC 3261
//! §19.3), full or compact form.

use super::{header_value, up_to_delimiter};

/// The `tag` parameter of the To header (full or compact `t` form), or the
/// empty string when absent (e.g. a tagless 100 Trying).
pub fn to_tag(raw: &[u8]) -> String {
    header_value(raw, "to")
        .or_else(|| header_value(raw, "t"))
        .map_or_else(String::new, |v| tag_of(&v))
}

/// The `tag` parameter of the From header (full or compact `f` form), or the
/// empty string when absent.
pub fn from_tag(raw: &[u8]) -> String {
    header_value(raw, "from")
        .or_else(|| header_value(raw, "f"))
        .map_or_else(String::new, |v| tag_of(&v))
}

/// The `tag` parameter of one From or To value, or the empty string. The
/// name is found in an ASCII-folded copy, whose byte offsets are the value's.
fn tag_of(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let Some(rest) = lower.find("tag=").and_then(|pos| value.get(pos + "tag=".len()..)) else {
        return String::new();
    };
    up_to_delimiter(rest, &[';', ',', ' ', '\t', '>']).trim().to_string()
}
