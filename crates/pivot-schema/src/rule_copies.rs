//! The files outside Rust that restate the validator's rule list, each written
//! from [`RfcRule::ALL`]: the token fixture the TypeScript mirror is tested
//! against, and that mirror's `RFC_RULES` array. A copy is never edited by
//! hand; [`REGEN`] rewrites every stale one.

use std::path::{Path, PathBuf};

use crate::violation::RfcRule;

/// The command that rewrites every copy, run from this repository's root.
pub const REGEN: &str = "just rule-copies";

const TOKENS: &str = "crates/pivot-schema/tests/fixtures/rfc-rule-tokens.txt";
const VIOLATION_TS: &str = "ts/contracts/src/violation.ts";
const TS_OPEN: &str = "export const RFC_RULES = [\n";
const TS_CLOSE: &str = "] as const";

/// One copy: where it lives and what the registry says it holds.
#[derive(Debug)]
pub struct RuleCopy {
    pub path: PathBuf,
    pub expected: String,
}

impl RuleCopy {
    /// Whether the file on disk differs from [`RuleCopy::expected`].
    pub fn is_stale(&self) -> bool {
        std::fs::read_to_string(&self.path).ok().as_deref() != Some(self.expected.as_str())
    }

    /// Rewrite the file to [`RuleCopy::expected`].
    pub fn write(&self) -> Result<(), String> {
        std::fs::write(&self.path, &self.expected)
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// The repository root this crate was built from.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every copy under `root`, with the content [`RfcRule::ALL`] gives it.
pub fn copies(root: &Path) -> Result<Vec<RuleCopy>, String> {
    let ts_path = root.join(VIOLATION_TS);
    let ts =
        std::fs::read_to_string(&ts_path).map_err(|e| format!("{}: {e}", ts_path.display()))?;
    Ok(vec![
        RuleCopy { path: root.join(TOKENS), expected: tokens_fixture() },
        RuleCopy { path: ts_path.clone(), expected: with_ts_rules(&ts, &ts_path)? },
    ])
}

/// The token fixture: one token per line, in [`RfcRule::ALL`] order.
pub fn tokens_fixture() -> String {
    RfcRule::ALL.iter().map(|r| format!("{}\n", r.token())).collect()
}

/// `ts` with its `RFC_RULES` array body replaced by the registry's tokens.
fn with_ts_rules(ts: &str, path: &Path) -> Result<String, String> {
    let missing = || format!("{}: no `{}…{TS_CLOSE}` block", path.display(), TS_OPEN.trim_end());
    let open = ts.find(TS_OPEN).ok_or_else(missing)? + TS_OPEN.len();
    let close = open + ts[open..].find(TS_CLOSE).ok_or_else(missing)?;
    let rows: Vec<String> = RfcRule::ALL.iter().map(|r| format!("  \"{}\"", r.token())).collect();
    Ok(format!("{}{}\n{}", &ts[..open], rows.join(",\n"), &ts[close..]))
}

/// The stale copies under `root`, as one report naming each and [`REGEN`];
/// `None` when every copy is current.
pub fn stale_report(root: &Path) -> Result<Option<String>, String> {
    let stale: Vec<String> = copies(root)?
        .iter()
        .filter(|c| c.is_stale())
        .map(|c| format!("  {}", c.path.strip_prefix(root).unwrap_or(&c.path).display()))
        .collect();
    Ok((!stale.is_empty())
        .then(|| format!("stale rule-list copies:\n{}\nregenerate: {REGEN}", stale.join("\n"))))
}
