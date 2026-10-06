//! The counts of a fixed family keyed by its label values: one lock-free
//! counter per declared label set, looked up by the values a caller names.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::family::{Family, Kind};
use crate::open::{key, with_key};

/// A fixed family's counts, one per declared label set, written at 0 before
/// their first observation. Share behind an `Arc`.
#[derive(Debug)]
pub struct FixedCounts {
    family: &'static Family,
    slots: HashMap<String, usize>,
    counts: Vec<AtomicU64>,
}

impl FixedCounts {
    /// The counts of `family`, all 0. Panics when `family` is semi-open, a
    /// histogram, or declares one label set twice (two blocks of a union).
    pub fn new(family: &'static Family) -> Self {
        assert!(!family.is_semi_open(), "{} is semi-open: use OpenRows", family.name);
        assert!(family.kind != Kind::Histogram, "{} is a histogram: no counts", family.name);
        let mut slots = HashMap::new();
        family.labels.for_each_series(|s| {
            let at = slots.len();
            let k = key(s.labels().map(|(_, v)| v));
            let dup = slots.insert(k, at);
            assert!(dup.is_none(), "{} declares a label set twice", family.name);
        });
        let counts = (0..slots.len()).map(|_| AtomicU64::new(0)).collect();
        Self { family, slots, counts }
    }

    /// Add `n` to the label set `values`. A label set the family does not
    /// declare is a caller's bug: refused in a debug build, not counted.
    pub fn add(&self, values: &[&str], n: u64) {
        match with_key(values, |k| self.slots.get(k).copied()) {
            Some(at) => {
                self.counts[at].fetch_add(n, Ordering::Relaxed);
            }
            None => debug_assert!(false, "{} declares no {values:?}", self.family.name),
        }
    }

    /// The count of the label set `values`, 0 when it is not declared.
    pub fn get(&self, values: &[&str]) -> u64 {
        with_key(values, |k| self.slots.get(k).copied())
            .map_or(0, |at| self.counts[at].load(Ordering::Relaxed))
    }

    /// The sum of every count.
    pub fn sum(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    /// Append the family.
    pub fn render(&self, out: &mut String) {
        let mut at = 0;
        self.family.render(out, |_| {
            at += 1;
            self.counts[at - 1].load(Ordering::Relaxed)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dim, Labels};

    const KIND: Dim = Dim::new("kind", &["a", "b"]);
    const F: Family = Family::counter("k_total", Labels::Product(&[KIND]), "by kind");

    #[test]
    fn every_declared_set_renders_with_its_own_count() {
        let c = FixedCounts::new(&F);
        c.add(&["b"], 2);
        assert_eq!(c.get(&["b"]), 2);
        assert_eq!(c.sum(), 2);
        let mut s = String::new();
        c.render(&mut s);
        assert!(s.ends_with("k_total{kind=\"a\"} 0\nk_total{kind=\"b\"} 2\n"), "{s}");
        assert_eq!(F.check(&s), Ok(()));
    }

    #[test]
    #[should_panic(expected = "declares no")]
    fn an_undeclared_set_is_a_bug() {
        FixedCounts::new(&F).add(&["c"], 1);
    }

    #[test]
    #[should_panic(expected = "twice")]
    fn a_union_declaring_a_label_set_twice_is_refused() {
        const TWICE: Family =
            Family::counter("t_total", Labels::Union(&[&[KIND], &[KIND]]), "twice");
        FixedCounts::new(&TWICE);
    }

    #[test]
    #[should_panic(expected = "histogram")]
    fn a_histogram_has_no_counts() {
        const H: Family = Family::histogram("h_seconds", Labels::None, "h");
        FixedCounts::new(&H);
    }
}
