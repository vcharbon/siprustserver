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

#[cfg(test)]
mod tests {
    use super::find_subslice;

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
}
