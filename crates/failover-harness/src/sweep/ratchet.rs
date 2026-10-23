//! The ratchet: the sweep cells that fail today, each with the decision that
//! owns its correction (ADR-0055 §11). One line per cell, its name then its
//! owner — an ADR and its section, `ADR-0055 §3` — and an optional comment
//! after `#`; blank lines and `#` lines are comments. A missing file lists
//! nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The listed failing cells, by name, with their owners.
pub struct Ratchet {
    path: PathBuf,
    cells: BTreeMap<String, String>,
}

impl Ratchet {
    /// Read the ratchet at `path`. Panics on a line that names no owner.
    pub fn load(path: impl AsRef<Path>) -> Ratchet {
        let path = path.as_ref().to_path_buf();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let cells = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        Ratchet { path, cells }
    }

    /// Hold the run of the cells under `prefix` to the ratchet: `ran` is every
    /// cell run, `failed` the failing ones with their findings. Panics naming
    /// each failing cell the ratchet does not list, each listed cell that
    /// passed and each listed cell that no longer exists.
    pub fn hold(&self, prefix: &str, ran: &[String], failed: &BTreeMap<String, String>) {
        let listed: Vec<&String> = self.cells.keys().filter(|c| c.starts_with(prefix)).collect();
        let mut wrong = Vec::new();
        for (cell, finding) in failed {
            if !self.cells.contains_key(cell) {
                wrong.push(format!(
                    "a new failure, not on the ratchet: {cell}\n    {}",
                    first_line(finding)
                ));
            }
        }
        for cell in &listed {
            if !ran.contains(cell) {
                wrong.push(format!("a listed cell that no longer exists: {cell}"));
            } else if !failed.contains_key(*cell) {
                wrong.push(format!(
                    "a listed cell that passes, to remove from the ratchet: {cell} ({})",
                    self.cells[*cell]
                ));
            }
        }
        for (cell, finding) in failed.iter().filter(|(c, _)| self.cells.contains_key(*c)) {
            eprintln!("listed failure {cell} ({}): {}", self.cells[cell], first_line(finding));
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

/// The cells a ratchet's text lists, with their owners.
fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut cells = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (cell, owner) = line
            .split_once(char::is_whitespace)
            .map(|(c, o)| (c, o.trim()))
            .ok_or_else(|| format!("line {}: a cell with no owner", n + 1))?;
        if !owner.starts_with("ADR-") || !owner.contains('§') {
            return Err(format!("line {}: the owner {owner:?} names no ADR section", n + 1));
        }
        if cells.insert(cell.to_string(), owner.to_string()).is_some() {
            return Err(format!("line {}: {cell} is listed twice", n + 1));
        }
    }
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratchet(text: &str) -> Ratchet {
        Ratchet { path: "test.ratchet".into(), cells: parse(text).unwrap() }
    }

    fn failing(cells: &[&str]) -> BTreeMap<String, String> {
        cells.iter().map(|c| (c.to_string(), "finding".to_string())).collect()
    }

    fn ran(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn a_line_names_a_cell_and_its_owner() {
        let cells = parse("# head\n\ns/i/t1-left  ADR-0055 §3  # why\n").unwrap();
        assert_eq!(cells.get("s/i/t1-left").map(String::as_str), Some("ADR-0055 §3"));
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
        r.hold("s/i/", &ran(&["s/i/t1-left", "s/i/t2-left"]), &failing(&["s/i/t1-left"]));
    }

    #[test]
    #[should_panic(expected = "a new failure")]
    fn an_unlisted_failure_fails() {
        ratchet("").hold("s/i/", &ran(&["s/i/t1-left"]), &failing(&["s/i/t1-left"]));
    }

    #[test]
    #[should_panic(expected = "passes, to remove")]
    fn a_listed_cell_that_passes_fails() {
        ratchet("s/i/t1-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"]), &failing(&[]));
    }

    #[test]
    #[should_panic(expected = "no longer exists")]
    fn a_listed_cell_that_is_gone_fails() {
        ratchet("s/i/t9-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"]), &failing(&[]));
    }

    #[test]
    fn other_prefixes_are_not_judged() {
        ratchet("s/j/t1-left ADR-0055 §3\n").hold("s/i/", &ran(&["s/i/t1-left"]), &failing(&[]));
    }
}
