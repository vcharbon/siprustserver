//! Compact header form expansion: the forms of RFC 3261 §7.3.3, Event's `o`
//! and Allow-Events' `u` (RFC 6665 §8.2.1), Refer-To's `r` (RFC 3515 §2.1),
//! Referred-By's `b` (RFC 3892 §3) and Session-Expires's `x` (RFC 4028 §4).
//! This table is the one every reader of a header name consults.

use super::scanner::Span;
use crate::sip_str::{SharedText, SipStr};

/// Expand a single-character compact form to its canonical name; otherwise
/// return the wire name unchanged. Neither arm allocates: a canonical name is
/// a `'static` literal, an unexpanded name stays a span of the image.
pub fn expand_compact_form(text: &SharedText, name: Span) -> SipStr {
    match text.as_str().get(name.clone()).and_then(canonical_of) {
        Some(canonical) => SipStr::from_static(canonical),
        None => text.span(name.start, name.len()),
    }
}

/// Expand a compact form on a plain name — the borrowed twin of
/// [`expand_compact_form`] for callers that hold no message image.
pub fn expanded_name(name: &str) -> &str {
    canonical_of(name).unwrap_or(name)
}

/// The canonical long form a §7.3.3 compact name expands to, or `None` when
/// `name` is not a compact form — the public probe over the one table below,
/// so a consumer that must PUBLISH the mapping enumerates it rather than
/// keeping a second copy.
pub fn compact_form_canonical(name: &str) -> Option<&'static str> {
    canonical_of(name)
}

/// The canonical long form of a single-character compact name, its letter
/// compared ignoring ASCII case.
fn canonical_of(name: &str) -> Option<&'static str> {
    let [letter] = name.as_bytes() else {
        return None;
    };
    match letter.to_ascii_lowercase() {
        b'b' => Some("Referred-By"),
        b'c' => Some("Content-Type"),
        b'e' => Some("Content-Encoding"),
        b'f' => Some("From"),
        b'i' => Some("Call-ID"),
        b'k' => Some("Supported"),
        b'l' => Some("Content-Length"),
        b'm' => Some("Contact"),
        b'o' => Some("Event"),
        b'r' => Some("Refer-To"),
        b's' => Some("Subject"),
        b't' => Some("To"),
        b'u' => Some("Allow-Events"),
        b'v' => Some("Via"),
        b'x' => Some("Session-Expires"),
        _ => None,
    }
}
