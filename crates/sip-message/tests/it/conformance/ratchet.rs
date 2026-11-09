//! The rows a run failed, against the list of those known to fail. A row that
//! fails and is not listed fails the suite, and so does a listed row that no
//! longer fails: the list only shrinks. `ratchet.txt` holds one row per line, its
//! name then the area of the stack that owns the fix.

use std::collections::{BTreeMap, BTreeSet};

/// The areas of the stack a listed row waits on.
const OWNERS: &[&str] =
    &["one-reader", "case-and-framing", "multipart-and-media-type", "offer-answer", "audit"];

/// Every family a test settles: a listed row of any other family answers to no
/// test, and fails the suite.
const FAMILIES: &[&str] = &[
    "abnf",
    "abnf-long",
    "frozen",
    "rfc",
    "must",
    "refuse",
    "recase",
    "body",
    "sdp",
    "exempt",
    "readable",
];

/// The list as written: `(row, owner)`, comments and blank lines left out.
fn entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    include_str!("ratchet.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split_once(' ').unwrap_or_else(|| panic!("row and owner: {line}")))
}

/// One row of a table and how it ended.
#[derive(Debug)]
pub struct Row {
    pub name: String,
    pub failure: Option<String>,
}

impl Row {
    pub fn new(name: impl Into<String>, outcome: Result<(), String>) -> Self {
        Self { name: name.into(), failure: outcome.err() }
    }
}

/// The rows a sweep of many values failed, each with its count and first sample.
#[derive(Default)]
pub struct Failures(BTreeMap<String, (usize, String)>);

impl Failures {
    /// `input` of `target` failed `row` for `reason`: the row is `<target>/<row>`,
    /// so a refusal one target makes never hides another's.
    pub fn note(&mut self, row: &str, target: &str, input: &str, reason: &str) {
        let entry = self.0.entry(format!("{target}/{row}")).or_default();
        entry.0 += 1;
        if entry.1.is_empty() {
            let sample: String = input.chars().take(120).collect();
            entry.1 = format!("{sample:?} -> {reason}");
        }
    }

    fn failing(&self, family: &str) -> BTreeMap<String, String> {
        let row = |(name, (count, sample)): (&String, &(usize, String))| {
            (format!("{family}/{name}"), format!("{count} values, first {sample}"))
        };
        self.0.iter().map(row).collect()
    }
}

fn listed(family: &str) -> BTreeSet<&'static str> {
    assert!(FAMILIES.contains(&family), "{family} is no family of the suite");
    let prefix = format!("{family}/");
    entries().map(|(row, _)| row).filter(|row| row.starts_with(&prefix)).collect()
}

fn unlisted(failing: &BTreeMap<String, String>, listed: &BTreeSet<&str>) -> Vec<String> {
    let new = failing.iter().filter(|(name, _)| !listed.contains(name.as_str()));
    new.map(|(name, why)| format!("FAILS, NOT LISTED {name} ({why})")).collect()
}

/// What `failing` and `listed` disagree on, both ways.
fn disagreement(failing: &BTreeMap<String, String>, listed: &BTreeSet<&str>) -> Vec<String> {
    let mut report = unlisted(failing, listed);
    let gone = listed.iter().filter(|row| !failing.contains_key(**row));
    report.extend(gone.map(|row| format!("LISTED, NO LONGER FAILS {row}")));
    report
}

#[track_caller]
fn hold(family: &str, failing: &BTreeMap<String, String>) {
    let report = disagreement(failing, &listed(family));
    assert!(report.is_empty(), "{family}, against ratchet.txt:\n{}", report.join("\n"));
}

/// Holds the rows of a table, which are every row of `family`, to the list.
#[track_caller]
pub fn settle(family: &str, rows: &[Row]) {
    let prefix = format!("{family}/");
    let mut seen = BTreeSet::new();
    let mut failing = BTreeMap::new();
    for row in rows {
        assert!(row.name.starts_with(&prefix), "{} is no row of {family}", row.name);
        assert!(seen.insert(row.name.as_str()), "{} is named twice", row.name);
        if let Some(why) = &row.failure {
            failing.insert(row.name.clone(), why.clone());
        }
    }
    hold(family, &failing);
}

/// Holds the rows a sweep failed to the list.
#[track_caller]
pub fn settle_rows(family: &str, failures: &Failures) {
    hold(family, &failures.failing(family));
}

#[test]
fn the_list_names_each_row_once_with_a_known_owner() {
    let mut owners: BTreeMap<&str, usize> = BTreeMap::new();
    let mut rows = BTreeSet::new();
    for (row, owner) in entries() {
        assert!(rows.insert(row), "{row} is listed twice");
        let family = row.split('/').next().unwrap_or_default();
        assert!(FAMILIES.contains(&family), "{row}: no test settles {family}");
        assert!(!row.ends_with("/unexplained"), "{row}: an unexplained refusal is named first");
        assert!(OWNERS.contains(&owner), "{row}: no owner named {owner}");
        *owners.entry(owner).or_default() += 1;
    }
    eprintln!("ratchet: {} rows {owners:?}", rows.len());
}

#[test]
fn a_new_failure_and_a_listed_row_that_passes_each_fail_the_suite() {
    let failing = BTreeMap::from([("t/new".to_string(), "why".to_string())]);
    let listed = BTreeSet::from(["t/fixed"]);
    let report = disagreement(&failing, &listed);
    assert_eq!(
        report,
        ["FAILS, NOT LISTED t/new (why)", "LISTED, NO LONGER FAILS t/fixed"].map(str::to_string)
    );
    let held = BTreeMap::from([("t/held".to_string(), "why".to_string())]);
    assert!(disagreement(&held, &BTreeSet::from(["t/held"])).is_empty());
}

/// Every family has a test that settles it.
#[test]
fn every_family_is_settled_by_a_test() {
    let sources = [
        include_str!("generated.rs"),
        include_str!("../abnf_fuzz.rs"),
        include_str!("rfc_examples.rs"),
        include_str!("must_statements/mod.rs"),
        include_str!("break_rule.rs"),
        include_str!("bodies.rs"),
        include_str!("exemption_mode.rs"),
        include_str!("readable_rows.rs"),
    ];
    for family in FAMILIES {
        let settles = [format!("settle(\"{family}\""), format!("settle_rows(\"{family}\"")];
        let found = sources.iter().any(|text| settles.iter().any(|call| text.contains(call)));
        assert!(found, "no test settles {family}");
    }
}
