//! The **cell result record** (ADR-0018): one serde `RunResult` per cell —
//! verdict, check verdicts, the RFC findings, the neutral [`SeqDoc`] diagram,
//! timings — written as `<cell dir>/result.json` in camelCase, the byte form
//! `ts/contracts/src/e2e.ts` decodes.

use scenario_harness::RunReport;
use seq_report::{Anomaly, SeqDoc};
use serde::{Deserialize, Serialize};

use e2e_model::checks::{self, CheckVerdict};

/// One cell of the campaign matrix: {Test case × Callflow shape × Infra shape}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CellId {
    pub case: String,
    pub shape: String,
    pub infra: String,
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
    /// Check verdicts all passed AND the run's expects held AND no extra
    /// anomaly of the run gates (an HTTP service's verdict) AND the RFC hard
    /// gate found no gating (non-advisory, subject-applicable, unwaived)
    /// violation. A gating RFC violation FAILS the cell with the report intact
    /// (`rfc` rows carry `advisory: false`) instead of crashing it report-less.
    pub passed: bool,
    pub checks: Vec<CheckVerdict>,
    /// The RFC findings of the run: the diagram's anomalies the audit rule
    /// registry sourced (`rule_sourced`), each tagged with its endpoint and
    /// advisory/gating severity. Structural recorder findings and harness
    /// verdicts (an HTTP service's, a failed expect) stay in `seq_doc` only.
    pub rfc: Vec<Anomaly>,
    /// The neutral sequence diagram — render with `seq_report::render_svg`.
    pub seq_doc: SeqDoc,
    pub timings: Timings,
}

impl RunResult {
    /// Fold a finished run + its check verdicts + the RFC hard-gate findings
    /// into the persistable result. `rfc_gate` is what `Harness::finish` would
    /// have panicked on — non-empty ⇒ the cell FAILS (the findings themselves
    /// are already in the seq-doc fold as `advisory: false` rows).
    pub fn from_run(
        cell: CellId,
        report: &RunReport,
        check_verdicts: Vec<CheckVerdict>,
        rfc_gate: &[sip_net_testkit::RfcFinding],
    ) -> Self {
        let seq_doc = scenario_harness::report::seq_doc(report);
        let entries = report.entries();
        let timings = Timings {
            first_ms: entries.iter().map(|e| e.sent_ms).min().unwrap_or(0),
            last_ms: entries.iter().map(|e| e.received_ms.unwrap_or(e.sent_ms)).max().unwrap_or(0),
            messages: entries.len(),
        };
        let passed = report.passed() && checks::all_passed(&check_verdicts) && rfc_gate.is_empty();
        RunResult {
            cell,
            passed,
            checks: check_verdicts,
            rfc: seq_doc.anomalies.iter().filter(|a| a.rule_sourced).cloned().collect(),
            seq_doc,
            timings,
        }
    }
}
