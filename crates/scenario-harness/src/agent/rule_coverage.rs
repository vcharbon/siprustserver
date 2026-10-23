//! The rule-coverage record of a finished run, under
//! `<target dir>/rule-coverage/<crate>.<binary>__<test>.tsv`, one file per
//! test. Each harness the test finishes appends one block: a header line
//! `# run=<nextest run id>\texe=<test binary>`, then one row per RFC rule the
//! run gave an occasion, `test, rule, occasions, decided, hits, waived`. The
//! test column reads `<crate>.<binary>::<test>`; `waived` names whose findings
//! of that rule a waiver covered in that run: `-`, `sut`, `simulated` or
//! `sut+simulated`. Written on every finish: the suite-wide census of which
//! rules the synthetic tests exercise is the point, and an opt-in census misses
//! tests. A test's first write in a process replaces its previous file.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The files this process has written, so the first write of a run truncates
/// a previous run's file and the next harness of the same test appends.
static WRITTEN: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Per rule, how many of its findings a waiver covered in the run, split by
/// whose findings they were.
pub(super) struct WaivedFindings<'a> {
    pub sut: &'a BTreeMap<String, usize>,
    pub simulated: &'a BTreeMap<String, usize>,
}

/// Record `population` for the running test. A run outside a cargo target dir
/// records nothing; a write error is reported on stderr and never fails the
/// run.
pub(super) fn record(
    harness: &str,
    population: &BTreeMap<&'static str, rfc_rules::Population>,
    waived: WaivedFindings<'_>,
) {
    let Some((dir, binary, exe)) = target_dir_and_binary() else { return };
    let test = test_name(harness);
    let path = dir.join("rule-coverage").join(format!("{}__{}.tsv", binary, sanitise(&test)));
    let run = std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| "-".to_string());
    let block = block(&run, &exe, &format!("{binary}::{test}"), population, &waived);
    if let Err(e) = write(&path, &block) {
        eprintln!("[harness] rule coverage not recorded at {}: {e}", path.display());
    }
}

/// One harness's block: the header line, then one row per rule.
fn block(
    run: &str,
    exe: &Path,
    test: &str,
    population: &BTreeMap<&'static str, rfc_rules::Population>,
    waived: &WaivedFindings<'_>,
) -> String {
    let mut out = format!("# run={run}\texe={}\n", exe.display());
    for (rule, p) in population {
        let by = match (waived.sut.contains_key(*rule), waived.simulated.contains_key(*rule)) {
            (false, false) => "-",
            (true, false) => "sut",
            (false, true) => "simulated",
            (true, true) => "sut+simulated",
        };
        out.push_str(&format!(
            "{test}\t{rule}\t{}\t{}\t{}\t{by}\n",
            p.occasions, p.decided, p.hits
        ));
    }
    out
}

/// Write `rows` to `path`: truncating on this process's first write to it,
/// appending after.
fn write(path: &Path, rows: &str) -> std::io::Result<()> {
    let first = WRITTEN.lock().unwrap().get_or_insert_with(HashSet::new).insert(path.to_owned());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!first)
        .truncate(first)
        .open(path)?;
    file.write_all(rows.as_bytes())
}

/// The cargo target dir, the test binary's `<crate>.<binary>` name and its
/// path, off the running executable's path
/// (`<target>/<profile>/deps/<binary>-<hash>`).
fn target_dir_and_binary() -> Option<(PathBuf, String, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?.to_owned();
    if deps.file_name()? != "deps" {
        return None;
    }
    let target = deps.parent()?.parent()?.to_owned();
    let stem = exe.file_stem()?.to_str()?;
    let binary = stem.rsplit_once('-').map_or(stem, |(name, _hash)| name);
    // Cargo and nextest name the test's package in its environment; every
    // crate's integration binary shares one name.
    let binary = match std::env::var("CARGO_PKG_NAME") {
        Ok(package) => format!("{package}.{binary}"),
        Err(_) => binary.to_string(),
    };
    Some((target, binary, exe))
}

/// The running test's name: libtest runs each test on a thread named after
/// it; elsewhere, the harness's own name.
fn test_name(harness: &str) -> String {
    match std::thread::current().name() {
        Some(name) if name != "main" => name.to_string(),
        _ => harness.to_string(),
    }
}

/// `name` with every character outside `[A-Za-z0-9_.-]` replaced by `_`.
fn sanitise(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn population(occasions: u64, decided: u64, hits: u64) -> rfc_rules::Population {
        rfc_rules::Population { occasions, decided, hits }
    }

    /// The block names the run and the binary, then one row per rule with
    /// whose findings of it a waiver covered; a rule no waiver covered reads
    /// `-` even when a waiver names it.
    #[test]
    fn a_block_states_the_run_the_binary_and_whose_findings_were_waived() {
        let rules = BTreeMap::from([
            ("cseq-in-dialog-order", population(4, 4, 1)),
            ("max-forwards", population(3, 3, 1)),
            ("mid-dialog-tags", population(2, 2, 2)),
            ("via-branch", population(5, 4, 0)),
        ]);
        let sut =
            BTreeMap::from([("max-forwards".to_string(), 1), ("mid-dialog-tags".to_string(), 1)]);
        let simulated = BTreeMap::from([
            ("cseq-in-dialog-order".to_string(), 1),
            ("mid-dialog-tags".to_string(), 1),
        ]);
        let waived = WaivedFindings { sut: &sut, simulated: &simulated };
        let text = block("r1", Path::new("/t/debug/deps/it-0"), "c.it::t", &rules, &waived);
        assert_eq!(
            text,
            "# run=r1\texe=/t/debug/deps/it-0\n\
             c.it::t\tcseq-in-dialog-order\t4\t4\t1\tsimulated\n\
             c.it::t\tmax-forwards\t3\t3\t1\tsut\n\
             c.it::t\tmid-dialog-tags\t2\t2\t2\tsut+simulated\n\
             c.it::t\tvia-branch\t5\t4\t0\t-\n"
        );
    }
}
