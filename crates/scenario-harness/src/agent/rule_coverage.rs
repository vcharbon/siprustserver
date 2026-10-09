//! The rule-coverage record of a finished run: one TSV row per RFC rule the
//! run gave an occasion, `test, rule, occasions, decided, hits, waived`, under
//! `<target dir>/rule-coverage/<crate>.<binary>__<test>.tsv`; the test column
//! reads `<crate>.<binary>::<test>`. Written on every finish
//! (one small file per test): the suite-wide census of which rules the
//! synthetic tests exercise is the point, and an opt-in census misses tests.
//! A test that finishes several harnesses appends one block per harness.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::waiver::WaiverState;

/// The files this process has written, so the first write of a run truncates
/// a previous run's file and the next harness of the same test appends.
static WRITTEN: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Record `population` for the running test, flagging each rule a declared
/// waiver names. A run outside a cargo target dir records nothing; a write
/// error is reported on stderr and never fails the run.
pub(super) fn record(
    harness: &str,
    population: &BTreeMap<&'static str, rfc_rules::Population>,
    waivers: &[WaiverState],
) {
    let Some((dir, binary)) = target_dir_and_binary() else { return };
    let test = test_name(harness);
    let path = dir.join("rule-coverage").join(format!("{}__{}.tsv", binary, sanitise(&test)));
    let waived: HashSet<&str> = waivers.iter().map(|w| w.scope.rule.as_str()).collect();
    let mut rows = String::new();
    for (rule, p) in population {
        let flag = u8::from(waived.contains(rule));
        rows.push_str(&format!(
            "{binary}::{test}\t{rule}\t{}\t{}\t{}\t{flag}\n",
            p.occasions, p.decided, p.hits
        ));
    }
    if let Err(e) = write(&path, &rows) {
        eprintln!("[harness] rule coverage not recorded at {}: {e}", path.display());
    }
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

/// The cargo target dir and the test binary's `<crate>.<binary>` name, off
/// the running executable's path (`<target>/<profile>/deps/<binary>-<hash>`).
fn target_dir_and_binary() -> Option<(PathBuf, String)> {
    let exe = std::env::current_exe().ok()?;
    let deps = exe.parent()?;
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
    Some((target, binary))
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
