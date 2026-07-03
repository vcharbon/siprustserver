//! The JSON **Load profile** (the loadgen⇄e2e fusion): the complete declarative
//! run spec for the load generator — the offered-rate/duration/concurrency knobs,
//! the sampling + reporting cadence, the global loss/retransmit defaults, the
//! agent recv bound, and the **scenario mix** (which shapes run, at what weight,
//! with what attached Test case and per-scenario loss/retransmit overrides).
//!
//! It is the load analogue of a [`Campaign`](crate::model::Campaign): a Campaign
//! expands a {case × shape × infra} matrix for the functional e2e runner; a Load
//! profile parameterizes one sustained load RUN. Both are dependency-light
//! authored documents validated at load, and `xtask e2e-schema` emits a
//! `load-profile.schema.json` alongside the others.
//!
//! Precedence at the CLI (documented on the `--load-profile` flag): the profile
//! supplies **defaults**; an explicitly-passed CLI flag **overrides** the profile
//! value. So a profile pins a repeatable baseline and a one-off `--cps` tweaks it
//! without editing the file.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::ModelError;

/// The complete declarative load-run spec. Every field carries a `#[serde(default)]`
/// so a partial profile is legal (its omitted fields fall back to the same
/// defaults the CLI flags carry), and the driver/CLI layer applies the
/// flag-overrides-profile precedence on top.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadProfile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Offered call rate (calls per second). The governor's *initial* target;
    /// `POST /rate` re-targets it live.
    #[serde(default = "default_cps")]
    pub cps: f64,
    /// How long to offer load, in seconds.
    #[serde(default = "default_duration_secs")]
    pub duration_secs: u64,
    /// Max concurrent in-flight calls (offered load above this is dropped+counted).
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,

    /// Max stored callflow samples per `(scenario × result-class)` bucket.
    #[serde(default = "default_sample_cap")]
    pub sample_cap: u32,
    /// Record roughly 1 call in N's flow (the background sampling fraction);
    /// `1` = full recording.
    #[serde(default = "default_background_record_every")]
    pub background_record_every: u64,
    /// Re-write the on-disk report every N seconds during the run (`0` = only at
    /// the end).
    #[serde(default)]
    pub report_interval_secs: u64,

    /// Per-`recv` wall-clock wait bound (ms) handed to every agent.
    #[serde(default = "default_recv_timeout_ms")]
    pub recv_timeout_ms: u64,

    /// The GLOBAL loss/retransmit defaults applied to every mix entry that has no
    /// per-scenario override.
    #[serde(default)]
    pub robustness: Robustness,

    /// The scenario MIX: which shapes run, their pick weights, and per-scenario
    /// attachments/overrides. Empty = the registry's shipped default mix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mix: Vec<MixSpec>,
}

/// The GLOBAL loss/retransmit defaults (a mix entry inherits these unless it
/// overrides them). Both default off, so an un-tuned profile is byte-for-byte the
/// historic behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Robustness {
    /// Simulated packet-drop probability on every call's mux legs (`0` = off).
    #[serde(default)]
    pub drop_rate: f64,
    /// Auto-retransmit lost signaling per real SIP timers, so a rare drop is
    /// recovered instead of failing the call.
    #[serde(default)]
    pub retransmit: bool,
}

/// One scenario-mix entry: the shape id + its pick weight, an optional attached
/// Test case (its binding pool drives per-call identities/dwells), and optional
/// per-scenario loss/retransmit overrides of the profile's global [`Robustness`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MixSpec {
    /// The shape's stable id (its `ShapeDescriptor` id / CLI `--scenario` name).
    pub shape: String,
    /// Pick weight (relative). Defaults to `1.0`.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Path to an authored Test-case JSON attached to THIS mix entry (overrides
    /// the run's global `--case`). Relative paths resolve against the process CWD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case: Option<PathBuf>,
    /// Per-scenario drop-rate override (absent = inherit the global
    /// [`Robustness::drop_rate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_rate: Option<f64>,
    /// Per-scenario retransmit override (absent = inherit the global
    /// [`Robustness::retransmit`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retransmit: Option<bool>,
}

fn default_cps() -> f64 {
    10.0
}
fn default_duration_secs() -> u64 {
    60
}
fn default_max_in_flight() -> usize {
    2000
}
fn default_sample_cap() -> u32 {
    10
}
fn default_background_record_every() -> u64 {
    64
}
fn default_recv_timeout_ms() -> u64 {
    5000
}
fn default_weight() -> f64 {
    1.0
}

impl Default for LoadProfile {
    fn default() -> Self {
        LoadProfile {
            schema: None,
            description: None,
            cps: default_cps(),
            duration_secs: default_duration_secs(),
            max_in_flight: default_max_in_flight(),
            sample_cap: default_sample_cap(),
            background_record_every: default_background_record_every(),
            report_interval_secs: 0,
            recv_timeout_ms: default_recv_timeout_ms(),
            robustness: Robustness::default(),
            mix: Vec::new(),
        }
    }
}

impl LoadProfile {
    /// Load a Load profile from a JSON file, with the same IO/parse error shape as
    /// the other authored docs, then validate it (non-empty shape ids, positive
    /// rates, coherent overrides). A malformed profile fails loudly at load.
    pub fn load(path: &std::path::Path) -> Result<Self, ModelError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ModelError::Io { path: path.to_path_buf(), source })?;
        let profile: LoadProfile = serde_json::from_str(&text)
            .map_err(|source| ModelError::Parse { path: path.to_path_buf(), source })?;
        profile.validate()?;
        Ok(profile)
    }

    /// Load-time validation: rates/durations non-negative, drop probabilities in
    /// `[0,1]`, and every mix entry names a non-empty shape with a positive
    /// weight. Returns **all** problems at once (the model-wide convention).
    pub fn validate(&self) -> Result<(), ModelError> {
        let mut problems = Vec::new();
        if self.cps.is_nan() || self.cps < 0.0 {
            problems.push(format!("cps must be >= 0 (got {})", self.cps));
        }
        if !in_unit(self.robustness.drop_rate) {
            problems.push(format!(
                "robustness.dropRate must be in [0,1] (got {})",
                self.robustness.drop_rate
            ));
        }
        for (i, entry) in self.mix.iter().enumerate() {
            if entry.shape.trim().is_empty() {
                problems.push(format!("mix[{i}]: shape id is empty"));
            }
            if entry.weight.is_nan() || entry.weight <= 0.0 {
                problems.push(format!(
                    "mix[{i}] ({:?}): weight must be > 0 (got {})",
                    entry.shape, entry.weight
                ));
            }
            if let Some(dr) = entry.drop_rate {
                if !in_unit(dr) {
                    problems.push(format!(
                        "mix[{i}] ({:?}): dropRate must be in [0,1] (got {dr})",
                        entry.shape
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ModelError::Invalid(problems))
        }
    }
}

/// `x ∈ [0,1]` (and not NaN — `contains` is false for NaN).
fn in_unit(x: f64) -> bool {
    (0.0..=1.0).contains(&x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal profile parses, filling every omitted knob with the same default
    /// the CLI flags carry (the additive-optional guarantee).
    #[test]
    fn minimal_profile_fills_defaults() {
        let p: LoadProfile = serde_json::from_str(r#"{ "cps": 25 }"#).unwrap();
        assert_eq!(p.cps, 25.0);
        assert_eq!(p.duration_secs, default_duration_secs());
        assert_eq!(p.max_in_flight, default_max_in_flight());
        assert_eq!(p.sample_cap, default_sample_cap());
        assert_eq!(p.background_record_every, default_background_record_every());
        assert_eq!(p.report_interval_secs, 0);
        assert_eq!(p.recv_timeout_ms, default_recv_timeout_ms());
        assert_eq!(p.robustness, Robustness::default());
        assert!(p.mix.is_empty());
        p.validate().expect("a minimal profile is valid");
    }

    /// A full profile with a mix round-trips and validates; per-scenario overrides
    /// and an attached case survive the round-trip.
    #[test]
    fn full_profile_round_trips_and_validates() {
        let json = r#"{
            "description": "endurance baseline",
            "cps": 20,
            "durationSecs": 3600,
            "maxInFlight": 8000,
            "sampleCap": 50,
            "backgroundRecordEvery": 1,
            "reportIntervalSecs": 60,
            "recvTimeoutMs": 5000,
            "robustness": { "dropRate": 0.001, "retransmit": true },
            "mix": [
                { "shape": "basic_call", "weight": 16 },
                { "shape": "reinvite", "weight": 4, "dropRate": 0.002 },
                { "shape": "options_hold", "weight": 1, "retransmit": false },
                { "shape": "refer", "weight": 1, "case": "e2e/cases/load-basic-pooled.json" }
            ]
        }"#;
        let p: LoadProfile = serde_json::from_str(json).unwrap();
        p.validate().expect("valid");
        assert_eq!(p.mix.len(), 4);
        assert_eq!(p.mix[1].drop_rate, Some(0.002));
        assert_eq!(p.mix[2].retransmit, Some(false));
        assert_eq!(p.mix[3].case.as_deref(), Some(std::path::Path::new("e2e/cases/load-basic-pooled.json")));

        // Round-trip (defaults are re-applied on the omitted `weight` in mix[0]).
        let back: LoadProfile = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    /// An unknown key is rejected (typo protection — `deny_unknown_fields`).
    #[test]
    fn unknown_key_is_rejected() {
        let err = serde_json::from_str::<LoadProfile>(r#"{ "cpss": 10 }"#);
        assert!(err.is_err(), "a typo'd key must fail to parse");
    }

    /// Validation surfaces EVERY problem: a bad global drop rate, a negative cps,
    /// an empty shape id, a non-positive weight, and a per-scenario drop out of range.
    #[test]
    fn validate_reports_all_problems() {
        let p: LoadProfile = serde_json::from_str(
            r#"{
                "cps": -1,
                "robustness": { "dropRate": 2 },
                "mix": [
                    { "shape": "", "weight": 1 },
                    { "shape": "basic_call", "weight": 0 },
                    { "shape": "reinvite", "weight": 1, "dropRate": -0.5 }
                ]
            }"#,
        )
        .unwrap();
        let err = p.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cps must be >= 0"), "{msg}");
        assert!(msg.contains("robustness.dropRate"), "{msg}");
        assert!(msg.contains("mix[0]"), "{msg}");
        assert!(msg.contains("mix[1]"), "{msg}");
        assert!(msg.contains("mix[2]"), "{msg}");
    }
}
