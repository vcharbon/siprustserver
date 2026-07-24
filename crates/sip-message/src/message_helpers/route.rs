//! Route / Record-Route value readers: loose-route detection and
//! strict-route URI extraction (RFC 3261 §12.2.1.1 / §16.12). Zero-regex
//! (ADR-0001). Computing the Request-URI + Route set of a *new* in-dialog
//! request lives in [`crate::generators`].

/// `true` if a Route / Record-Route header value carries the `;lr` loose-route
/// flag as a URI parameter — i.e. `;lr` followed by `;`, `>`, `,`, whitespace,
/// or end-of-string (not as a substring of some other token). Loose routing is
/// the modern default; strict routing is the legacy fallback.
pub fn first_route_is_loose(route_value: &str) -> bool {
    let lower = route_value.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(";lr") {
        let idx = from + rel;
        let after = idx + ";lr".len();
        if after == bytes.len()
            || matches!(bytes[after], b';' | b'>' | b',' | b' ' | b'\t' | b'\r' | b'\n')
        {
            return true;
        }
        from = after;
    }
    false
}

/// Extract the URI portion of a Route value for use as a strict-route
/// Request-URI: strips the surrounding angle brackets (RFC 3261 §16.12).
pub fn strip_route_uri_to_request_uri(route_value: &str) -> String {
    let trimmed = route_value.trim();
    if let Some(rest) = trimmed.strip_prefix('<') {
        if let Some(end) = rest.find('>') {
            return rest[..end].to_string();
        }
    }
    trimmed.to_string()
}
