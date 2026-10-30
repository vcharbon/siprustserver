//! Which calls of a load run ran recorded, and how far a failure judged on
//! recordings stands for the whole run.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The sampling gate's bounds, as the run was configured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sampling {
    /// Stored callflow samples per `(scenario, class, case)` bucket; also the
    /// first calls of each scenario that record.
    pub sample_cap: u32,
    /// 1 call of each scenario in this many records (`0`: none).
    pub record_every: u64,
}

/// Per scenario: the calls placed and recorded, and the calls the RFC audit
/// judged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioRecording {
    pub scenario: String,
    /// Calls placed, as the sampling gate counts them.
    pub placed: u64,
    /// Calls that ran recorded.
    pub recorded: u64,
    /// Calls that ended on an accepted branch before the RFC audit: the calls
    /// the audit judges.
    pub accepted: u64,
    /// Accepted calls whose recording the audit judged.
    pub audited: u64,
}

/// The recorded calls a failure row's class judges on their recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum JudgedBasis {
    /// `rfc_audit_fail`: the accepted calls whose recording the audit judged,
    /// standing for every accepted call.
    Audited,
    /// `check_fail`: the calls whose case checks ran, standing for every
    /// accepted call clean of the audit.
    Checked,
}

/// A failure row's share of the recorded calls its class judged, and the
/// estimate it gives over every call that class would judge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JudgedShare {
    /// Judged calls that failed this row.
    pub failed: u64,
    /// Recorded calls the class judged.
    pub judged: u64,
    /// Every call the class would judge.
    pub base: u64,
    pub basis: JudgedBasis,
}

impl JudgedShare {
    /// `failed / judged`, in `[0, 1]` (`0` when nothing was judged).
    pub fn share(&self) -> f64 {
        if self.judged == 0 {
            0.0
        } else {
            self.failed as f64 / self.judged as f64
        }
    }

    /// The share applied to the base, rounded.
    pub fn estimate(&self) -> u64 {
        (self.share() * self.base as f64).round() as u64
    }
}
