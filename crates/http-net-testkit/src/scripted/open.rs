//! The open-match rule: which live script a tokenless request opens.
//!
//! Candidates are the unopened instances whose open (and, for a reified
//! script, step 0's expect) matched the request. Each states a set of
//! fragments; set A is at least as specific as set B when every fragment of B
//! is covered by one of A: the same fragment up to capture names, or, for a
//! fragment of B with a capture, a literal fragment of A it matches (a stated
//! value is more specific than a wildcard on the same key). The maximal sets
//! win: one equivalence class opens its earliest instance (FIFO), several
//! incomparable ones are an ambiguity. A repeated request may therefore open a
//! broader sibling once the narrower one is consumed: it is a request the
//! broader script states.

use super::matcher;
use super::program::HttpBindings;
use super::template::{self, Piece};

/// One fragment of an open, bindings resolved.
#[derive(Clone, Debug)]
pub(super) struct Fragment {
    /// Its text with capture names erased: two fragments differing only in
    /// capture names are the same predicate.
    key: String,
    /// Its pieces when it holds a capture.
    pattern: Option<Vec<Piece>>,
}

impl Fragment {
    /// `entry` (a checked template) with `bindings` resolved.
    pub(super) fn new(entry: &str, bindings: &HttpBindings) -> Self {
        let pieces: Vec<Piece> = template::parse(entry)
            .unwrap_or_else(|_| vec![Piece::Lit(entry.to_string())])
            .into_iter()
            .map(|p| match p {
                Piece::Bind(name) => Piece::Lit(bindings.get(&name).unwrap_or("").to_string()),
                other => other,
            })
            .collect();
        let key = pieces
            .iter()
            .map(|p| match p {
                Piece::Lit(text) => text.as_str(),
                _ => "${capture}",
            })
            .collect();
        let pattern = pieces.iter().any(|p| matches!(p, Piece::Capture(_))).then_some(pieces);
        Self { key, pattern }
    }

    fn covers(&self, other: &Fragment) -> bool {
        self.key == other.key
            || (self.pattern.is_none()
                && other.pattern.as_ref().is_some_and(|p| matcher::pattern_matches(p, &self.key)))
    }
}

/// One instance whose open matched.
pub(super) struct Candidate {
    pub instance: u64,
    pub fragments: Vec<Fragment>,
}

/// The rule's outcome.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pick {
    None,
    One(u64),
    Ambiguous(Vec<u64>),
}

fn at_least(a: &[Fragment], b: &[Fragment]) -> bool {
    b.iter().all(|fb| a.iter().any(|fa| fa.covers(fb)))
}

/// Apply the rule to `candidates`, given in the order they were added.
pub(super) fn pick(candidates: &[Candidate]) -> Pick {
    let above = |a: &Candidate, b: &Candidate| {
        at_least(&a.fragments, &b.fragments) && !at_least(&b.fragments, &a.fragments)
    };
    let maximal: Vec<&Candidate> =
        candidates.iter().filter(|c| !candidates.iter().any(|d| above(d, c))).collect();
    let Some(first) = maximal.first() else {
        return Pick::None;
    };
    if maximal.iter().all(|c| {
        at_least(&c.fragments, &first.fragments) && at_least(&first.fragments, &c.fragments)
    }) {
        Pick::One(first.instance)
    } else {
        Pick::Ambiguous(maximal.iter().map(|c| c.instance).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(instance: u64, fragments: &[&str]) -> Candidate {
        let bindings = HttpBindings::new();
        Candidate {
            instance,
            fragments: fragments.iter().map(|s| Fragment::new(s, &bindings)).collect(),
        }
    }

    #[test]
    fn identical_sets_open_the_earliest() {
        assert_eq!(pick(&[c(4, &["a"]), c(2, &["a"])]), Pick::One(4));
        assert_eq!(
            pick(&[c(4, &[r#""k":"${capture:x}""#]), c(2, &[r#""k":"${capture:y}""#])]),
            Pick::One(4),
            "capture names do not matter"
        );
    }

    #[test]
    fn the_proper_superset_wins_over_what_it_nests() {
        assert_eq!(pick(&[c(0, &["a"]), c(1, &["a", "b"]), c(2, &[])]), Pick::One(1));
    }

    #[test]
    fn a_literal_is_more_specific_than_a_capture_it_matches() {
        let cap = r#""k":"${capture:x}""#;
        assert_eq!(pick(&[c(0, &[cap]), c(1, &[r#""k":"v""#])]), Pick::One(1));
        assert_eq!(
            pick(&[c(0, &[cap, r#""z":1"#]), c(1, &[r#""k":"v""#])]),
            Pick::Ambiguous(vec![0, 1]),
            "each states something the other does not"
        );
    }

    #[test]
    fn incomparable_maximal_sets_are_ambiguous() {
        assert_eq!(pick(&[c(0, &["a"]), c(1, &["b"]), c(2, &[])]), Pick::Ambiguous(vec![0, 1]));
        assert_eq!(pick(&[]), Pick::None);
    }
}
