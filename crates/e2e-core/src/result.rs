//! The **result model + persistence** (ADR-0018 Phase F): one serde
//! `RunResult` per cell — verdict, check verdicts, surfaced report findings,
//! the neutral [`SeqDoc`] diagram, timings — written as
//! `e2e/runs/<campaign>/<ts>/<case>__<shape>__<infra>/result.json`, with a
//! `campaign.json` aggregate index per run. JSON-first: the web layer renders
//! these files; heavy artifacts (`.wav`) are sibling files, never inlined.

use std::io;
use std::path::{Path, PathBuf};

use scenario_harness::RunReport;
use seq_report::{Anomaly, SeqDoc};
use serde::{Deserialize, Serialize};

use crate::checks::{self, CheckVerdict};

/// One cell of the campaign matrix: {Test case × Callflow shape × Infra shape}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellId {
    pub case: String,
    pub shape: String,
    pub infra: String,
}

impl CellId {
    /// The cell's directory name under the run dir.
    pub fn dir_name(&self) -> String {
        format!("{}__{}__{}", self.case, self.shape, self.infra)
    }
}

/// Recorded-activity span — virtual ms under a paused clock, wall ms under a
/// real one (deliberately the recording's own timeline, so it is deterministic
/// where the run is).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timings {
    pub first_ms: u64,
    pub last_ms: u64,
    pub messages: usize,
}

/// Everything one cell run produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub cell: CellId,
    /// Check verdicts all passed AND the run's expects held. Non-advisory RFC
    /// violations never reach here — the harness hard-gates them at `finish()`
    /// (the run panics, the executor records the cell as crashed).
    pub passed: bool,
    pub checks: Vec<CheckVerdict>,
    /// Findings the report surfaces alongside the diagram (advisory/structural;
    /// same fold as the HTML report's anomaly list).
    pub rfc: Vec<Anomaly>,
    /// The neutral sequence diagram — render with `seq_report::render_svg`.
    pub seq_doc: SeqDoc,
    pub timings: Timings,
}

impl RunResult {
    /// Fold a finished run + its check verdicts into the persistable result.
    pub fn from_run(cell: CellId, report: &RunReport, check_verdicts: Vec<CheckVerdict>) -> Self {
        let seq_doc = scenario_harness::report::seq_doc(report);
        let entries = report.entries();
        let timings = Timings {
            first_ms: entries.iter().map(|e| e.sent_ms).min().unwrap_or(0),
            last_ms: entries
                .iter()
                .map(|e| e.received_ms.unwrap_or(e.sent_ms))
                .max()
                .unwrap_or(0),
            messages: entries.len(),
        };
        let passed = report.passed() && checks::all_passed(&check_verdicts);
        RunResult {
            cell,
            passed,
            checks: check_verdicts,
            rfc: seq_doc.anomalies.clone(),
            seq_doc,
            timings,
        }
    }
}

/// Per-cell line of the `campaign.json` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellSummary {
    pub cell: CellId,
    pub passed: bool,
    /// The cell directory (relative to the run dir) holding `result.json`.
    pub dir: String,
    /// Set when the cell CRASHED (a harness assertion / the RFC hard gate
    /// panicked) — there is then no `result.json`, only `error.txt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The aggregate index of one campaign run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CampaignIndex {
    pub campaign: String,
    /// Run timestamp label (the `<ts>` path segment) — supplied by the caller
    /// so the core stays clock-free.
    pub ts: String,
    pub cells: Vec<CellSummary>,
}

impl CampaignIndex {
    pub fn passed(&self) -> bool {
        self.cells.iter().all(|c| c.passed)
    }
}

/// `<runs_root>/<campaign>/<ts>` — the directory of one campaign run.
pub fn run_dir(runs_root: &Path, campaign: &str, ts: &str) -> PathBuf {
    runs_root.join(campaign).join(ts)
}

/// Persist one cell result as `<run_dir>/<cell>/result.json`. Creates the dir.
pub fn write_result(run_dir: &Path, result: &RunResult) -> io::Result<PathBuf> {
    let dir = run_dir.join(result.cell.dir_name());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("result.json");
    let json = serde_json::to_string_pretty(result).map_err(io::Error::other)?;
    std::fs::write(&path, json + "\n")?;
    Ok(path)
}

/// Read a cell result back from its directory.
pub fn read_result(cell_dir: &Path) -> io::Result<RunResult> {
    let text = std::fs::read_to_string(cell_dir.join("result.json"))?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

/// Persist the aggregate as `<run_dir>/campaign.json`.
pub fn write_campaign_index(run_dir: &Path, index: &CampaignIndex) -> io::Result<PathBuf> {
    std::fs::create_dir_all(run_dir)?;
    let path = run_dir.join("campaign.json");
    let json = serde_json::to_string_pretty(index).map_err(io::Error::other)?;
    std::fs::write(&path, json + "\n")?;
    Ok(path)
}

/// Read the aggregate back.
pub fn read_campaign_index(run_dir: &Path) -> io::Result<CampaignIndex> {
    let text = std::fs::read_to_string(run_dir.join("campaign.json"))?;
    serde_json::from_str(&text).map_err(io::Error::other)
}
