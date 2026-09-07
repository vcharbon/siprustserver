//! The **differential record kind**: one call driven identically at two (or
//! more) stacks, folded into a persistable comparison — per-lane outcome,
//! per-lane RFC audit, and the verdict the process exit rides. JSON-first like
//! [`crate::result`]; the driver that probed the stacks writes it, the data
//! plane renders it.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One lane's caller-side outcome, in the probe's own vocabulary. The reroute
/// fields are absent on a basic probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneOutcome {
    /// The caller saw a 2xx and the call completed and tore down.
    pub answered: bool,
    /// The alternate landed on a target DISTINCT from the primary — decided on
    /// parsed URI values, never on their text. Absent on a basic probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerouted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ruri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternate_ruri: Option<String>,
    /// The R-URI the b-leg reached the callee with; `None` when none arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b_leg_ruri: Option<String>,
    /// One human line, as the report printed it.
    pub summary: String,
}

/// One RFC-audit finding over a lane's recorded trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditFinding {
    pub rule: String,
    pub lane: String,
    pub detail: String,
    pub advisory: bool,
}

/// One lane's RFC verdict over its recorded caller trace. An empty or absent
/// trace is not clean — nothing was audited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneAudit {
    pub recorded: bool,
    pub entries: usize,
    pub findings: Vec<AuditFinding>,
    pub clean: bool,
}

/// One probed stack: its label, what it did, and what the audit says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneProbe {
    pub label: String,
    pub outcome: LaneOutcome,
    pub audit: LaneAudit,
}

/// The fold the process exit rides: agreement across lanes AND every audit clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DifferentialVerdict {
    pub agree: bool,
    pub rfc_clean: bool,
    pub passed: bool,
    /// The disagreement, in the report's words; absent when `agree`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disagreement: Option<String>,
}

/// Everything one differential run produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DifferentialResult {
    /// The number the probes dialed, as dialed.
    pub dialed: String,
    /// The probe shape — an open token (e.g. `basic`, `reroute`).
    pub mode: String,
    pub lanes: Vec<LaneProbe>,
    pub verdict: DifferentialVerdict,
}

/// Persist one differential result at `path`. Creates parent directories.
pub fn write_differential(path: &Path, result: &DifferentialResult) -> io::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(result).map_err(io::Error::other)?;
    std::fs::write(path, json + "\n")?;
    Ok(path.to_path_buf())
}

/// Read a differential result back.
pub fn read_differential(path: &Path) -> io::Result<DifferentialResult> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DifferentialResult {
        DifferentialResult {
            dialed: "+19999991660010001".to_string(),
            mode: "reroute".to_string(),
            lanes: vec![LaneProbe {
                label: "reference".to_string(),
                outcome: LaneOutcome {
                    answered: true,
                    rerouted: Some(true),
                    primary_ruri: Some("sip:a@h".to_string()),
                    alternate_ruri: Some("sip:b@h".to_string()),
                    b_leg_ruri: Some("sip:b@h".to_string()),
                    summary: "answered via alternate".to_string(),
                },
                audit: LaneAudit { recorded: true, entries: 14, findings: vec![], clean: true },
            }],
            verdict: DifferentialVerdict {
                agree: true,
                rfc_clean: true,
                passed: true,
                disagreement: None,
            },
        }
    }

    #[test]
    fn round_trips_through_its_file() {
        let dir =
            std::env::temp_dir().join(format!("differential-record-{}", std::process::id()));
        let path = dir.join("differential.json");
        write_differential(&path, &sample()).unwrap();
        assert_eq!(read_differential(&path).unwrap(), sample());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_basic_probe_serializes_without_the_reroute_fields() {
        let mut result = sample();
        result.mode = "basic".to_string();
        result.lanes[0].outcome.rerouted = None;
        result.lanes[0].outcome.primary_ruri = None;
        result.lanes[0].outcome.alternate_ruri = None;
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("rerouted"));
        assert!(!json.contains("primaryRuri"));
    }
}
