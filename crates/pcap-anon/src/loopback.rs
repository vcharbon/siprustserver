//! The Call-ID derivation an application server leaves when it re-originates
//! a call: leg D's Call-ID is `<non-empty prefix> + leg B's Call-ID`. The
//! index ([`crate::index`]) reads it off every leg, and the pairing proposals
//! `source.txt` presents ([`crate::propose`]) start from it.

/// Minimum base Call-ID length for the derivation relation (mirrors the
/// upstream correlation pass): a short base id is too easily a suffix of an
/// unrelated one.
pub(crate) const MIN_BASE_LEN: usize = 8;

/// Whether `derived` is a Call-ID an application server minted from `base`: a
/// non-empty prefix ahead of a base long enough that the suffix match is
/// evidence rather than coincidence.
pub(crate) fn derives(base: &str, derived: &str) -> bool {
    base.len() >= MIN_BASE_LEN && derived.len() > base.len() && derived.ends_with(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_ahead_of_a_long_enough_base_is_a_derivation() {
        assert!(derives("abcdefgh@host", "2-abcdefgh@host"));
        assert!(!derives("abcdefgh@host", "abcdefgh@host"), "no prefix");
        assert!(!derives("ab@h", "2-ab@h"), "a base shorter than MIN_BASE_LEN");
        assert!(!derives("abcdefgh@host", "zzzzzzzz@host"), "no suffix");
    }
}
