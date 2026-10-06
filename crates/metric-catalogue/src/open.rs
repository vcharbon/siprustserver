//! The observed label sets of a semi-open family and their values, bounded
//! by the family's cap.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::family::{Cap, Family, OVERFLOW};
use crate::labels::Dim;

/// Separates label values in a row key; no label value carries it.
const SEP: char = '\u{1f}';

/// A semi-open family's rows: one value per observed label set, the declared
/// ones written at 0 before their first observation. An undeclared label set
/// holding an undeclared value of a capped label lands, past the family's
/// cap, on an overflow row (those values [`OVERFLOW`], the others kept) and
/// is counted on the cap's overflow family. Clone the
/// handle behind an `Arc` to share it.
#[derive(Debug)]
pub struct OpenRows {
    family: &'static Family,
    rows: Mutex<Rows>,
}

#[derive(Debug, Default)]
struct Rows {
    values: BTreeMap<String, u64>,
    /// Capped undeclared rows held under their own labels, the cap's measure.
    undeclared: usize,
    /// Observations that landed on the overflow row.
    overflowed: u64,
}

impl OpenRows {
    /// The rows of `family`, none observed. Panics when `family` is fixed.
    pub fn new(family: &'static Family) -> Self {
        assert!(family.is_semi_open(), "{} is fixed: it has no open rows", family.name);
        if let Some(cap) = family.cap() {
            let blocks = family.labels.blocks();
            for label in cap.labels {
                let named = blocks.iter().any(|dims| dims.iter().any(|d| d.name == *label));
                assert!(named, "{}'s cap names no label {label:?} of it", family.name);
            }
        }
        Self { family, rows: Mutex::new(Rows::default()) }
    }

    /// Add `n` to the row `values`, under the cap.
    pub fn add(&self, values: &[&str], n: u64) {
        self.update(values, true, |v| *v += n, n);
    }

    /// Add `n` to the row `values` whatever the cap: for a label set drawn
    /// from a population the caller bounds (the members of a cluster).
    pub fn add_pinned(&self, values: &[&str], n: u64) {
        self.update(values, false, |v| *v += n, n);
    }

    /// Replace every row with `rows` in one step, a reader seeing the old
    /// census or the new one: the gauge of a census taken whole, where a
    /// label set absent from it drops out. A census bounds its own rows, so
    /// the family carries no cap.
    pub fn replace<'a>(&self, rows: impl IntoIterator<Item = (Vec<&'a str>, u64)>) {
        assert!(self.family.cap().is_none(), "{} is capped: no census", self.family.name);
        let mut g = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        g.values.clear();
        g.undeclared = 0;
        g.overflowed = 0;
        for (values, v) in rows {
            self.update_locked(&mut g, &values, true, |slot| *slot = v, 1);
        }
    }

    fn update(&self, values: &[&str], capped: bool, apply: impl FnOnce(&mut u64), n: u64) {
        let mut g = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        self.update_locked(&mut g, values, capped, apply, n);
    }

    /// Apply to the row `values`. A new row holding an undeclared value of a
    /// capped label spends one unit of the cap; any other row, or a pinned one
    /// (`capped` false), spends none. Past the cap the observation lands on
    /// the overflow row: those values [`OVERFLOW`], every other one kept.
    fn update_locked(
        &self,
        g: &mut Rows,
        values: &[&str],
        capped: bool,
        apply: impl FnOnce(&mut u64),
        n: u64,
    ) {
        let mut apply = Some(apply);
        let hit = with_key(values, |k| match g.values.get_mut(k) {
            Some(v) => {
                apply.take().expect("applied once")(v);
                true
            }
            None => false,
        });
        let Some(apply) = apply.filter(|_| !hit) else { return };
        let k = key(values.iter().copied());
        let blocks: Vec<&[Dim]> =
            self.family.labels.blocks().into_iter().filter(|b| b.len() == values.len()).collect();
        let dims = self.family.labels.block_of_arity(values.len());
        // Whether the value at `at` sits in a capped label and no block of the
        // row's labels declares it there.
        let open = |cap: &Cap, at: usize, v: &str| {
            cap.labels.contains(&dims[at].name) && !blocks.iter().any(|b| b[at].values.contains(&v))
        };
        let cap = self
            .family
            .cap()
            .filter(|cap| capped && values.iter().enumerate().any(|(at, v)| open(cap, at, v)));
        match cap {
            Some(cap) if g.undeclared >= cap.max => {
                g.overflowed += n;
                let overflow = key(values.iter().enumerate().map(|(at, v)| {
                    if open(&cap, at, v) {
                        OVERFLOW
                    } else {
                        *v
                    }
                }));
                apply(g.values.entry(overflow).or_default());
            }
            _ => {
                if cap.is_some() {
                    g.undeclared += 1;
                }
                apply(g.values.entry(k).or_default());
            }
        }
    }

    /// The value of the row `values`, 0 when not observed.
    pub fn get(&self, values: &[&str]) -> u64 {
        let g = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        with_key(values, |k| g.values.get(k).copied()).unwrap_or(0)
    }

    /// The sum of every row.
    pub fn sum(&self) -> u64 {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).values.values().sum()
    }

    /// Every observed row, ordered by its label values.
    pub fn rows(&self) -> Vec<(Vec<String>, u64)> {
        let g = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        g.values.iter().map(|(k, v)| (k.split(SEP).map(str::to_owned).collect(), *v)).collect()
    }

    /// Observations that landed on the overflow row.
    pub fn overflowed(&self) -> u64 {
        self.rows.lock().unwrap_or_else(|e| e.into_inner()).overflowed
    }

    /// Append the family, then its cap's overflow family when it has one.
    pub fn render(&self, out: &mut String) {
        let (rows, overflowed) = {
            let g = self.rows.lock().unwrap_or_else(|e| e.into_inner());
            let rows: Vec<(Vec<String>, u64)> = g
                .values
                .iter()
                .map(|(k, v)| (k.split(SEP).map(str::to_owned).collect(), *v))
                .collect();
            (rows, g.overflowed)
        };
        self.family.render_rows(out, rows);
        if let Some(cap) = self.family.cap() {
            cap.overflow.render_value(out, overflowed);
        }
    }
}

thread_local! {
    /// The key of a row looked up, built here so a hit allocates nothing.
    static SCRATCH: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Run `f` on the key of `values`, built in a reused buffer.
pub(crate) fn with_key<R>(values: &[&str], f: impl FnOnce(&str) -> R) -> R {
    SCRATCH.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                buf.push(SEP);
            }
            buf.push_str(v);
        }
        f(&buf)
    })
}

pub(crate) fn key<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut k = String::new();
    for (i, v) in values.enumerate() {
        if i > 0 {
            k.push(SEP);
        }
        k.push_str(v);
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dim, Labels};

    const METHOD: Dim = Dim::new("method", &["INVITE", "BYE"]);
    const OPEN: Family =
        Family::counter("req_total", Labels::Product(&[METHOD]), "requests").semi_open();
    const OVERFLOWED: Family =
        Family::counter("req_overflow_total", Labels::None, "requests past the cap");
    const CAPPED: Family = Family::counter("req_total", Labels::Product(&[METHOD]), "requests")
        .capped_at(2, &OVERFLOWED, &["method"]);

    #[test]
    fn declared_rows_render_at_zero_and_undeclared_ones_after_them() {
        let rows = OpenRows::new(&OPEN);
        let mut s = String::new();
        rows.render(&mut s);
        assert_eq!(
            s,
            "# HELP req_total requests\n# TYPE req_total counter\n\
             req_total{method=\"INVITE\"} 0\nreq_total{method=\"BYE\"} 0\n"
        );
        rows.add(&["FOO"], 2);
        rows.add(&["BYE"], 1);
        rows.add(&["BAR"], 1);
        let mut s = String::new();
        rows.render(&mut s);
        assert!(s.ends_with(
            "req_total{method=\"INVITE\"} 0\nreq_total{method=\"BYE\"} 1\n\
             req_total{method=\"BAR\"} 1\nreq_total{method=\"FOO\"} 2\n"
        ));
        assert_eq!(OPEN.check(&s), Ok(()));
        assert_eq!(rows.sum(), 4);
        assert_eq!(rows.get(&["FOO"]), 2);
    }

    #[test]
    fn past_the_cap_undeclared_rows_land_on_the_overflow_row_and_are_counted() {
        let rows = OpenRows::new(&CAPPED);
        for m in ["A", "B", "C", "D", "A"] {
            rows.add(&[m], 1);
        }
        rows.add(&["INVITE"], 1);
        rows.add_pinned(&["P"], 1);
        assert_eq!(rows.get(&["A"]), 2);
        assert_eq!(rows.get(&["C"]), 0);
        assert_eq!(rows.get(&[OVERFLOW]), 2);
        assert_eq!(rows.overflowed(), 2);
        assert_eq!(rows.get(&["P"]), 1, "a pinned row ignores the cap");
        assert_eq!(rows.sum(), 7, "no observation is lost");
        let mut s = String::new();
        rows.render(&mut s);
        assert!(s.contains("req_total{method=\"_overflow\"} 2\n"), "{s}");
        assert!(s.ends_with("req_overflow_total 2\n"), "{s}");
        assert_eq!(CAPPED.check(&s), Ok(()));
        assert_eq!(OVERFLOWED.check(&s), Ok(()));
    }

    #[test]
    fn a_replaced_census_drops_the_rows_absent_from_it() {
        let rows = OpenRows::new(&OPEN);
        rows.replace([(vec!["FOO"], 3), (vec!["INVITE"], 1)]);
        rows.replace([(vec!["BAR"], 2)]);
        assert_eq!(rows.get(&["FOO"]), 0);
        assert_eq!(rows.get(&["INVITE"]), 0);
        assert_eq!(rows.get(&["BAR"]), 2);
    }

    #[test]
    #[should_panic(expected = "is fixed")]
    fn a_fixed_family_has_no_open_rows() {
        const FIXED: Family = Family::counter("f_total", Labels::None, "fixed");
        OpenRows::new(&FIXED);
    }

    /// Pinned rows never spend the cap: the undeclared budget is left whole
    /// for the capped rows, however many pinned ones came first.
    #[test]
    fn pinned_rows_leave_the_cap_whole() {
        let rows = OpenRows::new(&CAPPED);
        for p in ["P1", "P2", "P3"] {
            rows.add_pinned(&[p], 1);
        }
        rows.add(&["A"], 1);
        rows.add(&["B"], 1);
        assert_eq!(rows.get(&["A"]), 1);
        assert_eq!(rows.get(&["B"]), 1);
        assert_eq!(rows.overflowed(), 0, "the two capped rows fit the cap of two");
    }

    const SCOPE: Dim = Dim::new("scope", &["internal", "external"]);
    const PEER: Dim = Dim::new("peer", &[]);
    const PEER_OVERFLOWED: Family = Family::counter("p_overflow_total", Labels::None, "past");
    const PEERS: Family = Family::counter("p_total", Labels::Product(&[PEER, SCOPE]), "by peer")
        .capped_at(1, &PEER_OVERFLOWED, &["peer"]);

    /// The overflow row keeps every label whose value is declared and puts
    /// `_overflow` on the open ones only.
    #[test]
    fn the_overflow_row_keeps_the_declared_labels() {
        let rows = OpenRows::new(&PEERS);
        rows.add(&["10.0.0.1", "external"], 1);
        rows.add(&["10.0.0.2", "external"], 1);
        rows.add(&["10.0.0.3", "internal"], 1);
        assert_eq!(rows.get(&[OVERFLOW, "external"]), 1);
        assert_eq!(rows.get(&[OVERFLOW, "internal"]), 1);
        assert_eq!(rows.overflowed(), 2);
    }

    /// A census is replaced in one step: a reader never sees it half built.
    #[test]
    fn a_replaced_census_is_never_seen_half_built() {
        let rows = std::sync::Arc::new(OpenRows::new(&OPEN));
        let census: Vec<String> = (0..200).map(|i| format!("S{i}")).collect();
        let writer = {
            let (rows, census) = (rows.clone(), census.clone());
            std::thread::spawn(move || {
                for _ in 0..200 {
                    rows.replace(census.iter().map(|c| (vec![c.as_str()], 1)));
                }
            })
        };
        for _ in 0..2_000 {
            let n = rows.sum();
            assert!(n == 0 || n == 200, "a census seen half built: {n} of 200 rows");
        }
        writer.join().unwrap();
    }

    const CODE: Dim = Dim::new("code", &["200"]);
    const RESP_OVERFLOWED: Family = Family::counter("r_overflow_total", Labels::None, "past");
    const RESPONSES: Family =
        Family::counter("r_total", Labels::Product(&[METHOD, CODE]), "by method and code")
            .capped_at(1, &RESP_OVERFLOWED, &["method"]);

    /// Only an undeclared value of a capped label spends the cap: a code
    /// nobody declared, of a declared method, is bounded by the protocol and
    /// keeps its own row past it.
    #[test]
    fn an_undeclared_value_of_an_uncapped_label_spends_no_cap() {
        let rows = OpenRows::new(&RESPONSES);
        rows.add(&["INVITE", "299"], 1);
        rows.add(&["BYE", "699"], 1);
        rows.add(&["FOO", "200"], 1);
        rows.add(&["BAR", "200"], 1);
        assert_eq!(rows.get(&["INVITE", "299"]), 1);
        assert_eq!(rows.get(&["BYE", "699"]), 1);
        assert_eq!(rows.get(&["FOO", "200"]), 1);
        assert_eq!(rows.get(&[OVERFLOW, "200"]), 1, "the capped label alone reads _overflow");
        assert_eq!(rows.overflowed(), 1);
    }

    /// A cap naming a label the family does not have would bound nothing:
    /// refused at construction.
    #[test]
    #[should_panic(expected = "names no label")]
    fn a_cap_on_a_label_the_family_lacks_is_refused() {
        const MISSPELT: Family = Family::counter("m_total", Labels::Product(&[METHOD]), "m")
            .capped_at(1, &OVERFLOWED, &["methd"]);
        OpenRows::new(&MISSPELT);
    }

    const LADDER_A: Dim = Dim::new("ladder", &["a"]);
    const LADDER_B: Dim = Dim::new("ladder", &["b"]);
    const VERB_X: Dim = Dim::new("method", &["X"]);
    const VERB_Y: Dim = Dim::new("method", &["Y"]);
    const LADDERS: Family = Family::counter(
        "l_total",
        Labels::Union(&[&[LADDER_A, VERB_X], &[LADDER_B, VERB_Y]]),
        "by ladder",
    )
    .capped_at(1, &OVERFLOWED, &["method"]);

    /// A value declared in any block of the row's labels spends no cap.
    #[test]
    fn a_value_declared_in_another_block_spends_no_cap() {
        let rows = OpenRows::new(&LADDERS);
        rows.add(&["a", "Y"], 1);
        rows.add(&["b", "X"], 1);
        rows.add(&["a", "FOO"], 1);
        assert_eq!(rows.get(&["a", "FOO"]), 1, "the one undeclared method fits the cap");
        assert_eq!(rows.overflowed(), 0);
    }

    /// A census of a capped family is refused whatever the build.
    #[test]
    #[should_panic(expected = "is capped")]
    fn a_census_of_a_capped_family_is_refused() {
        OpenRows::new(&CAPPED).replace([(vec!["A"], 1)]);
    }
}
