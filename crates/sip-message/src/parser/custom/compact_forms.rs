//! RFC 3261 §7.3.3 — compact header form expansion. Port of
//! `src/sip/parsers/custom/compact-forms.ts`.

use super::scanner::Span;
use crate::sip_str::{SharedText, SipStr};

/// Expand a single-character compact form to its canonical name; otherwise
/// return the wire name unchanged. Neither arm allocates: a canonical name is
/// a `'static` literal, an unexpanded name stays a span of the image.
pub fn expand_compact_form(text: &SharedText, name: Span) -> SipStr {
    match canonical_of(&text.as_str()[name.start..name.end]) {
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

/// The canonical long form of a single-character compact name. A non-ASCII
/// char whose lowercase is a single char folds the same way the wire form did.
fn canonical_of(name: &str) -> Option<&'static str> {
    let mut it = name.chars();
    let (Some(c), None) = (it.next(), it.next()) else {
        return None;
    };
    let mut lower_it = c.to_lowercase();
    let lower = match (lower_it.next(), lower_it.next()) {
        (Some(l), None) => l,
        _ => c,
    };
    match lower {
        'i' => Some("Call-ID"),
        'm' => Some("Contact"),
        'e' => Some("Content-Encoding"),
        'l' => Some("Content-Length"),
        'c' => Some("Content-Type"),
        'f' => Some("From"),
        's' => Some("Subject"),
        'k' => Some("Supported"),
        't' => Some("To"),
        'v' => Some("Via"),
        _ => None,
    }
}
