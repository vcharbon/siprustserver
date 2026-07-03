//! The machine-readable **Load run index** (`load-result.json`) — the loadgen
//! run's result as a single authored-shape document, so the e2e website can
//! render a load run beside a functional Campaign run without re-deriving
//! anything from Prometheus scrapes or the HTML report.
//!
//! The loadgen [`Reporter`] writes one of these next to `index.html` on every
//! periodic report rewrite AND at run end; the e2e-web `Load runs` section reads
//! it back (JSON-first — the HTML detail page is a pure projection of this doc,
//! so the two can never drift). Every path in it is **relative** to the run
//! directory (the `load-result.json` sits at the run-dir root), so a run dir is
//! self-contained and portable.
//!
//! `xtask e2e-schema` emits `load-run-index.schema.json` alongside the authored
//! docs. Unlike the authored inputs (Test case / Campaign / Load profile) this is
//! an OUTPUT document — nobody hand-writes it — so it carries no `$schema` field
//! and is not loaded/validated; the schema exists for consumers and the drift
//! test.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One load run's complete result, as persisted to `load-result.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadRunIndex {
    /// Run metadata: when it ran and the echoed knobs it ran with.
    pub meta: LoadRunMeta,
    /// Per-`(scenario, class, chaos)` completed-call counts (the report's main
    /// table). `class == "ok"` is the success bucket; `chaos == "near"` is
    /// accepted kill collateral, `chaos == "clear"` is a genuine result.
    pub counts: Vec<CountRow>,
    /// Per-scenario end-to-end latency summary (ms).
    pub latency: Vec<LatencyRow>,
    /// Per-`(scenario, checkpoint)` named-checkpoint latency summary (ms).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointRow>,
    /// Per-scenario Test-case check-verdict tally over the SAMPLED calls (the
    /// per-sample oracle): how many sampled calls passed all their checks vs
    /// failed at least one. Empty when no case with checks was attached.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckSummaryRow>,
    /// The run's health canaries — the cross-call signals a triager scans first.
    pub canaries: Canaries,
    /// Links to the stored sampled callflow pages, grouped by their
    /// `(scenario, class, chaos)` bucket. Paths are RELATIVE to the run dir.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<SampleGroup>,
}

/// Run metadata: timing and the echoed run knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadRunMeta {
    /// Wall-clock start, Unix epoch milliseconds.
    pub started_ms: i64,
    /// Wall-clock finish (the moment this doc was written), Unix epoch ms.
    pub finished_ms: i64,
    /// `false` while a periodic snapshot is written mid-run; `true` in the doc
    /// written at run end.
    pub finished: bool,
    /// The SUT ingress the INVITEs were routed through (the `lb`/VIP address).
    pub target: String,
    /// Offered call rate the run was configured for (calls/s).
    pub cps: f64,
    /// Configured run duration, seconds.
    pub duration_secs: u64,
    /// Max concurrent in-flight calls (offered load above this was shed).
    pub max_in_flight: u64,
    /// The egress policy label the run realized its b-leg with
    /// (`transparent` / `api-call-pin` / `registrar-aor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<String>,
    /// Optional load-profile description echoed from the run's `--load-profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// One `(scenario, class, chaos)` completed-call count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CountRow {
    pub scenario: String,
    /// The result class label (`ok`, `timeout`, `status_486`, `check_fail`, …).
    pub class: String,
    /// Chaos proximity: `clear` (genuine) or `near` (accepted kill collateral).
    pub chaos: String,
    pub count: u64,
    /// `true` iff `class == "ok"` (drives the OK-green / NOK-red split).
    pub ok: bool,
}

/// Per-scenario end-to-end latency summary (all values in milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LatencyRow {
    pub scenario: String,
    pub n: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Per-`(scenario, checkpoint)` named-checkpoint latency summary (milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointRow {
    pub scenario: String,
    pub checkpoint: String,
    pub n: u64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

/// Per-scenario Test-case check-verdict tally over the sampled calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckSummaryRow {
    pub scenario: String,
    /// Sampled calls whose every attached check passed.
    pub passed: u64,
    /// Sampled calls with at least one failing check (the `check_fail` class).
    pub failed: u64,
}

/// The run's cross-call health canaries — the first things a triager scans.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Canaries {
    /// Inbound datagrams that matched no live call (a post-failover stray-send
    /// burst is the classic SUT reclaim bug; should be ~0 in a clean run).
    pub orphans: u64,
    /// Offered calls dropped at the max-in-flight cap (offered load we shed).
    pub shed: u64,
    /// Datagrams discarded by the simulated packet-loss model (0 when loss off).
    pub drops: u64,
    /// Calls that reached the ring→answer step (the 18x-delivery denominator).
    pub ringing_expected: u64,
    /// Of those, how many saw the 18x ringing provisional (numerator). A dropped
    /// non-PRACK 18x is expected, so this is a RATE gated at >99%, not a per-call
    /// failure.
    pub ringing_received: u64,
}

impl Canaries {
    /// The 18x ringing-delivery ratio in `[0,1]` (`1.0` when nothing rang, so an
    /// all-200 run is not spuriously flagged). Below ~0.99 is a systemic 18x
    /// regression.
    pub fn ringing_ratio(&self) -> f64 {
        if self.ringing_expected == 0 {
            1.0
        } else {
            self.ringing_received as f64 / self.ringing_expected as f64
        }
    }
}

/// The stored sampled callflow pages for one `(scenario, class, chaos)` bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SampleGroup {
    pub scenario: String,
    pub class: String,
    pub chaos: String,
    /// Run-dir-relative paths to each sample's rendered callflow HTML page
    /// (e.g. `callflows/reinvite/check_fail/clear/0.html`).
    pub pages: Vec<String>,
}

impl LoadRunIndex {
    /// Total completed calls across every `(scenario, class, chaos)` bucket.
    pub fn total_calls(&self) -> u64 {
        self.counts.iter().map(|c| c.count).sum()
    }

    /// Completed calls that were NOT `ok` (the NOK total across all buckets).
    pub fn failed_calls(&self) -> u64 {
        self.counts.iter().filter(|c| !c.ok).map(|c| c.count).sum()
    }

    /// Genuine (chaos=clear) non-ok calls — the triage total that excludes
    /// accepted kill collateral.
    pub fn clear_failures(&self) -> u64 {
        self.counts
            .iter()
            .filter(|c| !c.ok && c.chaos == "clear")
            .map(|c| c.count)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> LoadRunIndex {
        LoadRunIndex {
            meta: LoadRunMeta {
                started_ms: 1_000,
                finished_ms: 61_000,
                finished: true,
                target: "172.20.255.250:5060".to_string(),
                cps: 20.0,
                duration_secs: 60,
                max_in_flight: 2000,
                egress: Some("transparent".to_string()),
                profile: Some("endurance baseline".to_string()),
            },
            counts: vec![
                CountRow {
                    scenario: "basic_call".into(),
                    class: "ok".into(),
                    chaos: "clear".into(),
                    count: 1180,
                    ok: true,
                },
                CountRow {
                    scenario: "reinvite".into(),
                    class: "check_fail".into(),
                    chaos: "clear".into(),
                    count: 3,
                    ok: false,
                },
                CountRow {
                    scenario: "reinvite".into(),
                    class: "timeout".into(),
                    chaos: "near".into(),
                    count: 2,
                    ok: false,
                },
            ],
            latency: vec![LatencyRow {
                scenario: "basic_call".into(),
                n: 1180,
                mean_ms: 12.5,
                p50_ms: 10.0,
                p90_ms: 25.0,
                p99_ms: 40.0,
                max_ms: 88.0,
            }],
            checkpoints: vec![CheckpointRow {
                scenario: "basic_call".into(),
                checkpoint: "ringing".into(),
                n: 1180,
                p50_ms: 3.0,
                p90_ms: 8.0,
                p99_ms: 15.0,
            }],
            checks: vec![CheckSummaryRow {
                scenario: "reinvite".into(),
                passed: 7,
                failed: 3,
            }],
            canaries: Canaries {
                orphans: 0,
                shed: 4,
                drops: 11,
                ringing_expected: 1185,
                ringing_received: 1184,
            },
            samples: vec![SampleGroup {
                scenario: "reinvite".into(),
                class: "check_fail".into(),
                chaos: "clear".into(),
                pages: vec!["callflows/reinvite/check_fail/clear/0.html".into()],
            }],
        }
    }

    /// The index round-trips through JSON byte-for-byte (the load-bearing
    /// property: the loadgen writes it and e2e-web reads it back verbatim).
    #[test]
    fn round_trips_through_json() {
        let idx = sample_index();
        let json = serde_json::to_string_pretty(&idx).unwrap();
        let back: LoadRunIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(back, idx);
    }

    /// camelCase field names are on the wire (the website/API read `startedMs`,
    /// `ringingReceived`, etc.).
    #[test]
    fn serializes_camel_case() {
        let json = serde_json::to_string(&sample_index()).unwrap();
        for key in [
            "\"startedMs\"",
            "\"finishedMs\"",
            "\"maxInFlight\"",
            "\"ringingExpected\"",
            "\"ringingReceived\"",
            "\"meanMs\"",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
    }

    /// The derived tallies (total / failed / clear-only) sum the buckets right.
    #[test]
    fn derived_totals() {
        let idx = sample_index();
        assert_eq!(idx.total_calls(), 1185);
        assert_eq!(idx.failed_calls(), 5, "3 check_fail + 2 timeout");
        assert_eq!(idx.clear_failures(), 3, "the near timeout is excused");
    }

    /// The ringing ratio is a rate, and an all-200 run (nothing rang) is 1.0.
    #[test]
    fn ringing_ratio_is_a_rate() {
        assert!((sample_index().canaries.ringing_ratio() - 1184.0 / 1185.0).abs() < 1e-9);
        assert_eq!(Canaries::default().ringing_ratio(), 1.0, "nothing rang → not flagged");
    }

    /// A schema is generated for the output doc (so `xtask e2e-schema` emits it
    /// and consumers can validate).
    #[test]
    fn has_a_json_schema() {
        let schema = schemars::schema_for!(LoadRunIndex);
        let json = serde_json::to_value(&schema).unwrap();
        assert!(json.get("properties").is_some(), "a schema object with properties");
    }
}
