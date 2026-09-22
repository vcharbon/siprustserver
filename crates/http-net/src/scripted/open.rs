//! The open-match rule: which live script a tokenless request opens.
//!
//! Candidates are the unopened instances whose open matched the request.
//! Among them the maximal fragment sets under inclusion win: one set (shared
//! by identical instances) opens its earliest instance; several incomparable
//! sets are an ambiguity.

use std::collections::BTreeSet;

/// One instance whose open matched.
pub(super) struct Candidate {
    pub instance: u64,
    /// The open's fragments, bindings resolved.
    pub fragments: BTreeSet<String>,
}

/// The rule's outcome.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pick {
    None,
    One(u64),
    Ambiguous(Vec<u64>),
}

/// Apply the rule to `candidates`, given in the order they were added.
pub(super) fn pick(candidates: &[Candidate]) -> Pick {
    let maximal: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| {
            !candidates.iter().any(|d| {
                d.fragments.len() > c.fragments.len() && d.fragments.is_superset(&c.fragments)
            })
        })
        .collect();
    let Some(first) = maximal.first() else {
        return Pick::None;
    };
    if maximal.iter().all(|c| c.fragments == first.fragments) {
        Pick::One(first.instance)
    } else {
        Pick::Ambiguous(maximal.iter().map(|c| c.instance).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(instance: u64, fragments: &[&str]) -> Candidate {
        Candidate { instance, fragments: fragments.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn identical_sets_open_the_earliest() {
        assert_eq!(pick(&[c(4, &["a"]), c(2, &["a"])]), Pick::One(4));
    }

    #[test]
    fn the_proper_superset_wins_over_what_it_nests() {
        assert_eq!(pick(&[c(0, &["a"]), c(1, &["a", "b"]), c(2, &[])]), Pick::One(1));
    }

    #[test]
    fn incomparable_maximal_sets_are_ambiguous() {
        assert_eq!(pick(&[c(0, &["a"]), c(1, &["b"]), c(2, &[])]), Pick::Ambiguous(vec![0, 1]));
        assert_eq!(pick(&[]), Pick::None);
    }
}
