//! A cell's `verdict.json` on disk: read, rewritten after a lane oracle failed
//! it, and its verification lines (`crate::verifications`) rewritten with it.

use std::path::Path;

use pivot_schema::bundle::{Failure as Finding, RunVerdict};

use crate::verifications::{self, Gates};
use crate::Failure;

/// The bundle file holding a cell's verdict.
pub const FILE: &str = "verdict.json";

/// Rewrites the bundle's `verdict.json` with `verdict`, and the lines it
/// splits into under `gates` (`verifications.json`).
pub fn write(out_dir: &Path, verdict: &RunVerdict, gates: &Gates) -> Result<(), Failure> {
    let text = pivot_schema::canonical::format(verdict)
        .map_err(|e| Failure::Environment(format!("serialize {FILE}: {e}")))?;
    std::fs::write(out_dir.join(FILE), text)
        .map_err(|e| Failure::Environment(format!("write {FILE}: {e}")))?;
    verifications::rewrite(out_dir, verdict, gates)
}

/// The verdict the bundle at `bundle` holds.
pub fn read(bundle: &Path) -> Result<RunVerdict, Failure> {
    let text = std::fs::read_to_string(bundle.join(FILE))
        .map_err(|e| Failure::Environment(format!("read {FILE}: {e}")))?;
    serde_json::from_str(&text).map_err(|e| Failure::Environment(format!("parse {FILE}: {e}")))
}

/// Fails the verdict the bundle at `bundle` holds with `finding`, written
/// back under `gates`, and answers the verdict as written.
pub fn charge(bundle: &Path, finding: Finding, gates: &Gates) -> Result<RunVerdict, Failure> {
    let mut verdict = read(bundle)?;
    verdict.fail(finding);
    write(bundle, &verdict, gates)?;
    Ok(verdict)
}
