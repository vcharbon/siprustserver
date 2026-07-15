//! Byte-slice search primitive shared by the pre-parse scanners.

/// First index of `needle` within `haystack` (byte substring search), or
/// `None`. An empty `needle` matches at 0; the callers here never pass one.
pub(super) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether `haystack` contains `needle`, comparing ASCII-case-insensitively.
pub(super) fn contains_subslice_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::{contains_subslice_ignore_ascii_case, find_subslice};

    #[test]
    fn finds_at_start_middle_and_absent() {
        assert_eq!(find_subslice(b"abcdef", b"abc"), Some(0));
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abcdef", b"xyz"), None);
    }

    #[test]
    fn empty_needle_matches_at_zero() {
        assert_eq!(find_subslice(b"abc", b""), Some(0));
        assert_eq!(find_subslice(b"", b""), Some(0));
    }

    #[test]
    fn needle_longer_than_haystack_is_none() {
        assert_eq!(find_subslice(b"ab", b"abc"), None);
    }

    #[test]
    fn case_insensitive_containment_ignores_ascii_case_only() {
        assert!(contains_subslice_ignore_ascii_case(b"xEsNeT.0y", b"esnet.0"));
        assert!(contains_subslice_ignore_ascii_case(b"abc", b""));
        assert!(!contains_subslice_ignore_ascii_case(b"esnet-0", b"esnet.0"));
        assert!(!contains_subslice_ignore_ascii_case(b"ab", b"abc"));
    }
}
