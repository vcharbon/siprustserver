//! The span-attribute size cap.
//!
//! A traced call records raw wire bytes; one oversized body must not become an
//! unbounded export payload. Every attribute is capped at [`ATTR_CAP_BYTES`]
//! and a capped attribute is marked so a reader never mistakes a prefix for the
//! whole value.

use std::borrow::Cow;

/// Maximum bytes any single span attribute carries.
pub const ATTR_CAP_BYTES: usize = 16 * 1024;

/// The span field set alongside a capped attribute.
pub const TRUNCATED_FIELD: &str = "truncated";

/// Cap a text attribute, returning the value and whether it was truncated.
/// Truncation lands on a `char` boundary, so the result is always valid UTF-8.
pub fn cap_str(value: &str) -> (Cow<'_, str>, bool) {
    if value.len() <= ATTR_CAP_BYTES {
        return (Cow::Borrowed(value), false);
    }
    let mut end = ATTR_CAP_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (Cow::Borrowed(&value[..end]), true)
}

/// Cap a raw byte attribute (a wire message), returning the value and whether
/// it was truncated.
pub fn cap_bytes(value: &[u8]) -> (&[u8], bool) {
    if value.len() <= ATTR_CAP_BYTES {
        (value, false)
    } else {
        (&value[..ATTR_CAP_BYTES], true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_at_the_cap_is_untouched() {
        let s = "x".repeat(ATTR_CAP_BYTES);
        let (out, truncated) = cap_str(&s);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(!truncated);
    }

    #[test]
    fn an_oversized_value_is_capped_and_marked() {
        let s = "x".repeat(ATTR_CAP_BYTES + 1);
        let (out, truncated) = cap_str(&s);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(truncated);
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        // A 3-byte char straddling the cap must be dropped whole.
        let mut s = "a".repeat(ATTR_CAP_BYTES - 1);
        s.push('€');
        let (out, truncated) = cap_str(&s);
        assert!(truncated);
        assert_eq!(out.len(), ATTR_CAP_BYTES - 1, "the straddling char is dropped whole");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn raw_bytes_cap_at_the_same_ceiling() {
        let b = vec![0xFFu8; ATTR_CAP_BYTES + 9];
        let (out, truncated) = cap_bytes(&b);
        assert_eq!(out.len(), ATTR_CAP_BYTES);
        assert!(truncated);
        let (out, truncated) = cap_bytes(&b[..8]);
        assert_eq!(out.len(), 8);
        assert!(!truncated);
    }
}
