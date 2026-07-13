//! RFC 3261 §7.3.3 — compact header form expansion. Port of
//! `src/sip/parsers/custom/compact-forms.ts`.

/// Expand a single-character compact form to its canonical name; otherwise
/// return the name unchanged.
pub fn expand_compact_form(name: &str) -> String {
    // Runs once per header per message: lowercase the single char directly
    // instead of minting a `String` just to probe the table. A non-ASCII char
    // whose lowercase is a single char keeps its historical expansion (the old
    // `to_lowercase()` comparison folded it the same way).
    let mut it = name.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        let mut lower_it = c.to_lowercase();
        let lower = match (lower_it.next(), lower_it.next()) {
            (Some(l), None) => l,
            _ => c,
        };
        let canonical = match lower {
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
        };
        if let Some(c) = canonical {
            return c.to_string();
        }
    }
    name.to_string()
}
