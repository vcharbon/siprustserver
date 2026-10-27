//! The ratchet: the sweep cells that fail today, each with the decision that
//! owns its correction (ADR-0055 §11), and the count of each test's cells in
//! the Guaranteed tier. One line per cell, its name then its owner — an ADR
//! and its section, `ADR-0055 §3` — and an optional comment after `#`, its
//! cause; one line `guaranteed <test> <count>` per test with a Guaranteed
//! cell. Blank lines and `#` lines are comments. A missing file lists
//! nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Ran;

/// The listed failing cells, by name, with their owners, and the listed
/// Guaranteed count of each test, by its cells' prefix.
pub struct Ratchet {
    path: PathBuf,
    cells: BTreeMap<String, String>,
    guaranteed: BTreeMap<String, usize>,
}

impl Ratchet {
    /// Read the ratchet at `path`. Panics on a line that names no owner.
    pub fn load(path: impl AsRef<Path>) -> Ratchet {
        let path = path.as_ref().to_path_buf();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let (cells, guaranteed) =
            parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Ratchet { path, cells, guaranteed }
    }

    /// Hold the run of the cells under `prefix` to the ratchet: `ran` is every
    /// cell run, with its failure if it failed, `guaranteed` the count of
    /// those in the Guaranteed tier, `dead` the dead windows the witness check
    /// found. Panics naming each failing cell the ratchet does not list, each
    /// listed cell that passed, each listed cell that no longer exists, a
    /// Guaranteed count other than the listed one, and each dead window.
    pub fn hold(&self, prefix: &str, ran: &[Ran], guaranteed: usize, dead: &[String]) {
        let failed: BTreeMap<&str, &str> =
            ran.iter().filter_map(|r| Some((r.name.as_str(), r.failure.as_deref()?))).collect();
        let listed: Vec<&String> = self.cells.keys().filter(|c| c.starts_with(prefix)).collect();
        let mut wrong = Vec::new();
        for (cell, finding) in &failed {
            if !self.cells.contains_key(*cell) {
                wrong.push(format!(
                    "a new failure, not on the ratchet: {cell}\n    {}",
                    first_line(finding)
                ));
            }
        }
        for cell in &listed {
            if !ran.iter().any(|r| &r.name == *cell) {
                wrong.push(format!("a listed cell that no longer exists: {cell}"));
            } else if !failed.contains_key(cell.as_str()) {
                wrong.push(format!(
                    "a listed cell that passes, to remove from the ratchet: {cell} ({})",
                    self.cells[*cell]
                ));
            }
        }
        let test = prefix.trim_end_matches('/');
        let listed_guaranteed = self.guaranteed.get(test).copied().unwrap_or(0);
        if guaranteed < listed_guaranteed {
            wrong.push(format!(
                "the Guaranteed cells of {test} fell from {listed_guaranteed} to {guaranteed}: \
                 a cell left the Guaranteed tier"
            ));
        } else if guaranteed > listed_guaranteed {
            wrong.push(format!(
                "the Guaranteed cells of {test} rose from {listed_guaranteed} to {guaranteed}: \
                 record `guaranteed {test} {guaranteed}`"
            ));
        }
        wrong.extend(dead.iter().cloned());
        for (cell, finding) in failed.iter().filter(|(c, _)| self.cells.contains_key(**c)) {
            eprintln!("listed failure {cell} ({}): {}", self.cells[*cell], first_line(finding));
        }
        assert!(
            wrong.is_empty(),
            "{} cells of {prefix}*, {} failing; the ratchet {} disagrees:\n{}",
            ran.len(),
            failed.len(),
            self.path.display(),
            wrong.join("\n")
        );
    }
}

/// The first line of a finding that says what failed.
fn first_line(finding: &str) -> &str {
    finding.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

/// The cells a ratchet's text lists, with their owners, and the Guaranteed
/// counts it lists, by test.
#[allow(clippy::type_complexity)]
fn parse(text: &str) -> Result<(BTreeMap<String, String>, BTreeMap<String, usize>), String> {
    let mut cells = BTreeMap::new();
    let mut guaranteed = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (cell, owner) = line
            .split_once(char::is_whitespace)
            .map(|(c, o)| (c, o.trim()))
            .ok_or_else(|| format!("line {}: a cell with no owner", n + 1))?;
        if cell == "guaranteed" {
            let (test, count) = owner
                .split_once(char::is_whitespace)
                .and_then(|(t, c)| Some((t, c.trim().parse::<usize>().ok()?)))
                .ok_or_else(|| format!("line {}: `guaranteed <test> <count>`", n + 1))?;
            if guaranteed.insert(test.to_string(), count).is_some() {
                return Err(format!("line {}: {test} is counted twice", n + 1));
            }
            continue;
        }
        if !owner.starts_with("ADR-") || !owner.contains('§') {
            return Err(format!("line {}: the owner {owner:?} names no ADR section", n + 1));
        }
        if cells.insert(cell.to_string(), owner.to_string()).is_some() {
            return Err(format!("line {}: {cell} is listed twice", n + 1));
        }
    }
    Ok((cells, guaranteed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::CellReport;
    use crate::turn_points::{TurnAt, TurnPoint};

    fn ratchet(text: &str) -> Ratchet {
        let (cells, guaranteed) = parse(text).unwrap();
        Ratchet { path: "test.ratchet".into(), cells, guaranteed }
    }

    /// The cells `all`, those of `failing` failing.
    fn ran(all: &[&str], failing: &[&str]) -> Vec<Ran> {
        let at = TurnAt { turn: 1, point: TurnPoint::Left };
        all.iter()
            .map(|c| Ran {
                name: c.to_string(),
                at,
                measured: CellReport::default(),
                failure: failing.contains(c).then(|| "finding".to_string()),
            })
            .collect()
    }

    #[test]
    fn a_line_names_a_cell_and_its_owner() {
        let (cells, _) = parse("# head\n\ns/i/t1-left  ADR-0055 §3  # why\n").unwrap();
        assert_eq!(cells.get("s/i/t1-left").map(String::as_str), Some("ADR-0055 §3"));
    }

    #[test]
    fn a_line_counts_a_tests_guaranteed_cells() {
        let (_, guaranteed) = parse("guaranteed s/applied/i 4\n").unwrap();
        assert_eq!(guaranteed.get("s/applied/i"), Some(&4));
        assert!(parse("guaranteed s/applied/i many\n").is_err());
        assert!(parse("guaranteed s/applied/i 4\nguaranteed s/applied/i 5\n").is_err());
    }

    #[test]
    fn a_line_without_an_owner_is_refused() {
        assert!(parse("s/i/t1-left\n").is_err());
        assert!(parse("s/i/t1-left  later\n").is_err());
        assert!(parse("s/i/t1-left ADR-0055 §3\ns/i/t1-left ADR-0055 §4\n").is_err());
    }

    #[test]
    fn listed_failures_hold() {
        let r = ratchet("s/i/t1-left ADR-0055 §3\n");
        r.hold("s/i/", &ran(&["s/i/t1-left", "s/i/t2-left"], &["s/i/t1-left"]), 0, &[]);
    }

    #[test]
    #[should_panic(expected = "a new failure")]
    fn an_unlisted_failure_fails() {
        ratchet("").hold("s/i/", &ran(&["s/i/t1-left"], &["s/i/t1-left"]), 0, &[]);
    }

    #[test]
    #[should_panic(expected = "passes, to remove")]
    fn a_listed_cell_that_passes_fails() {
        ratchet("s/i/t1-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 0, &[]);
    }

    #[test]
    #[should_panic(expected = "no longer exists")]
    fn a_listed_cell_that_is_gone_fails() {
        ratchet("s/i/t9-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 0, &[]);
    }

    #[test]
    fn other_prefixes_are_not_judged() {
        ratchet("s/j/t1-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 0, &[]);
    }

    #[test]
    #[should_panic(expected = "fell from 3 to 2")]
    fn a_guaranteed_count_that_falls_fails() {
        ratchet("guaranteed s/i 3\n").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 2, &[]);
    }

    #[test]
    #[should_panic(expected = "record `guaranteed s/i 2`")]
    fn a_guaranteed_count_that_rises_is_recorded() {
        ratchet("guaranteed s/i 1\n").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 2, &[]);
    }

    #[test]
    #[should_panic(expected = "a dead window")]
    fn a_dead_window_fails() {
        let dead = ["a dead window: W-x".to_string()];
        ratchet("").hold("s/i/", &ran(&["s/i/t1-left"], &[]), 0, &dead);
    }
}
