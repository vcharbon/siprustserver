//! RFC 3261 §7.3.3 — compact header form expansion. Port of
//! `src/sip/parsers/custom/compact-forms.ts`.

/// Expand a single-character compact form to its canonical name; otherwise
/// return the name unchanged.
pub fn expand_compact_form(name: &str) -> String {
    if name.chars().count() == 1 {
        let canonical = match name.to_lowercase().as_str() {
            "i" => Some("Call-ID"),
            "m" => Some("Contact"),
            "e" => Some("Content-Encoding"),
            "l" => Some("Content-Length"),
            "c" => Some("Content-Type"),
            "f" => Some("From"),
            "s" => Some("Subject"),
            "k" => Some("Supported"),
            "t" => Some("To"),
            "v" => Some("Via"),
            _ => None,
        };
        if let Some(c) = canonical {
            return c.to_string();
        }
    }
    name.to_string()
}
