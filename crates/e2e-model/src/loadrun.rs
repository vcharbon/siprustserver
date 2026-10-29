//! The machine-readable **Load run index** (`load-result.json`) — the loadgen
//! run's result as a single authored-shape document, so the e2e website can
//! render a load run beside a functional Campaign run without re-deriving
//! anything from Prometheus scrapes or the HTML report.
//!
//! The loadgen `Reporter` writes one of these next to `index.html` on every
//! periodic report rewrite AND at run end, and serves it live on its metrics
//! socket; the e2e-web `Load runs` section reads it back. Every page that shows
//! a load run is rendered from this doc alone (`loadrun-render`), so the live
//! page, the on-disk `index.html` and the website can never disagree. Every
//! path in it is **relative** to the run directory (the `load-result.json` sits
//! at the run-dir root), so a run dir is self-contained and portable.
//!
//! `xtask e2e-schema` emits `load-run-index.schema.json` alongside the authored
//! docs. Unlike the authored inputs (Test case / Campaign / Load profile) this is
//! an OUTPUT document — nobody hand-writes it — so it carries no `$schema` field
//! and is not loaded/validated; the schema exists for consumers and the drift
//! test.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod ladder;
mod progress;
mod sampling;
pub use ladder::{ClassCount, Ladder, LadderArrow};
pub use progress::{FailureRow, Progress, RateEvent, RateEventKind, SeriesPoint};
pub use sampling::{JudgedBasis, JudgedShare, Sampling, ScenarioRecording};

/// One load run's complete result, as persisted to `load-result.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadRunIndex {
    /// Run metadata: when it ran and the echoed knobs it ran with.
    pub meta: LoadRunMeta,
    /// The run's headline figures as of this doc.
    #[serde(default)]
    pub progress: Progress,
    /// The run's time series, oldest first: one point per window, every
    /// window of the run covered (adjacent windows merge as the run grows).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesPoint>,
    /// Every offered-rate change and drain, oldest first (bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_events: Vec<RateEvent>,
    /// Rate events past the bound, not listed in `rate_events`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rate_events_dropped: u64,
    /// The last failed calls, oldest first (bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_failures: Vec<FailureRow>,
    /// Per scenario: the calls placed, recorded and judged on their recording.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recording: Vec<ScenarioRecording>,
    /// Per scenario, the message ladder of its calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ladders: Vec<Ladder>,
    /// Per-`(scenario, class, case)` completed-call counts (the report's main
    /// table). A row's `ok` marks its class as no failure (`ok`: the call ended
    /// on an accepted branch).
    pub counts: Vec<CountRow>,
    /// Per-scenario end-to-end latency summary (ms).
    pub latency: Vec<LatencyRow>,
    /// Per-`(scenario, checkpoint)` named-checkpoint latency summary (ms).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointRow>,
    /// Per-`(scenario, exchange)` SIP round-trip summary (ms).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rtt: Vec<RttRow>,
    /// Per-scenario Test-case check-verdict tally over the SAMPLED calls (the
    /// per-sample oracle): how many sampled calls passed all their checks vs
    /// failed at least one. Empty when no case with checks was attached.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckSummaryRow>,
    /// The run's health canaries — the cross-call signals a triager scans first.
    pub canaries: Canaries,
    /// Links to the stored sampled callflow pages, grouped by their
    /// `(scenario, class, case)` bucket. Paths are RELATIVE to the run dir.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<SampleGroup>,
}

/// Run metadata: timing and the echoed run knobs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
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
    /// The load generator's command line, as typed (program first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_line: Vec<String>,
    /// The sampling gate's bounds: which calls ran recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Sampling>,
}

/// One `(scenario, class, case)` completed-call count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CountRow {
    pub scenario: String,
    /// The result class label (`ok`, `timeout`, `status_503`, `check_fail`, …).
    pub class: String,
    /// The bounded case discriminator refining the class into its distinct
    /// failure modes (RFC rule id(s), failed check id(s), the failed step
    /// `<who>[:<kind>#<index>]@<barrier>`).
    /// Empty = un-refined (`ok` rows, indexes written before the field existed).
    #[serde(default)]
    pub case: String,
    pub count: u64,
    /// `true` iff the class is no failure (`ok`). Drives the OK-green / NOK-red
    /// split.
    pub ok: bool,
    /// The row's share of the calls its class judges on their recording;
    /// `None` for a class judged on every call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged: Option<JudgedShare>,
    /// The first stored sample's one-line failure detail; `None` when the
    /// bucket stores no sample with a detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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

/// Per-`(scenario, exchange)` SIP round-trip summary (milliseconds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RttRow {
    pub scenario: String,
    /// The exchange timed (`invite_100`, `bye_final`, …).
    pub exchange: String,
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
    /// Inbound datagrams discarded by the simulated packet-loss model (0 when
    /// loss is off).
    pub drops_in: u64,
    /// Outbound datagrams discarded by the simulated packet-loss model.
    pub drops_out: u64,
    /// Correlated datagrams discarded because their call's inbox was full.
    #[serde(default)]
    pub inbox_drops: u64,
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

/// The stored sampled callflow pages for one `(scenario, class, case)` bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SampleGroup {
    pub scenario: String,
    pub class: String,
    /// The case discriminator (see [`CountRow::case`]); empty = un-refined.
    #[serde(default)]
    pub case: String,
    /// Run-dir-relative paths to each sample's rendered callflow HTML page
    /// (e.g. `callflows/reinvite/check_fail/alice.invite.from.userInfo/0.html`;
    /// no case segment when `case` is empty).
    pub pages: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl LoadRunIndex {
    /// Total completed calls across every `(scenario, class, case)` bucket.
    pub fn total_calls(&self) -> u64 {
        self.counts.iter().map(|c| c.count).sum()
    }

    /// Completed calls that were NOT `ok` (the NOK total across all buckets).
    pub fn failed_calls(&self) -> u64 {
        self.counts.iter().filter(|c| !c.ok).map(|c| c.count).sum()
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
                target: "192.0.2.250:5060".to_string(),
                cps: 20.0,
                duration_secs: 60,
                max_in_flight: 2000,
                egress: Some("transparent".to_string()),
                profile: Some("endurance baseline".to_string()),
                command_line: vec!["loadgen".into(), "--cps".into(), "20".into()],
                sampling: Some(Sampling { sample_cap: 10, record_every: 64, chase_calls: 64 }),
            },
            progress: Progress {
                elapsed_ms: 60_000,
                placed: 1185,
                ok: 1180,
                failed: 5,
                ..Progress::default()
            },
            series: vec![SeriesPoint {
                start_ms: 0,
                span_ms: 5_000,
                placed: 100,
                ok: 98,
                failed: 2,
                rejected: 0,
                retransmissions_received: 4,
                retransmissions_resent: 1,
                p50_ms: 10.0,
                p99_ms: 40.0,
            }],
            rate_events: vec![RateEvent {
                at_ms: 30_000,
                kind: RateEventKind::Rate,
                from_cps: 20.0,
                to_cps: 10.0,
            }],
            rate_events_dropped: 0,
            recent_failures: vec![FailureRow {
                at_ms: 12_000,
                scenario: "reinvite".into(),
                class: "timeout".into(),
                case: "bob@connected".into(),
                page: None,
            }],
            ladders: vec![Ladder {
                scenario: "reinvite".into(),
                calls: 5,
                engine: true,
                truncated: false,
                lanes: vec!["alice".into(), "SUT".into()],
                arrows: vec![LadderArrow {
                    from: "SUT".into(),
                    to: "alice".into(),
                    label: "180 (INVITE)".into(),
                    nth: 1,
                    count: 5,
                    retransmissions: 1,
                    resent: 0,
                    mean_offset_ms: 20.0,
                    errors: vec![ClassCount { class: "timeout".into(), count: 2 }],
                }],
                other: None,
                after_call: vec![ClassCount { class: "check_fail".into(), count: 3 }],
                before_reply: vec![],
            }],
            recording: vec![ScenarioRecording {
                scenario: "reinvite".into(),
                placed: 5,
                recorded: 5,
                accepted: 3,
                audited: 3,
            }],
            counts: vec![
                CountRow {
                    scenario: "basic_call".into(),
                    class: "ok".into(),
                    case: String::new(),
                    count: 1180,
                    ok: true,
                    judged: None,
                    error: None,
                },
                CountRow {
                    scenario: "reinvite".into(),
                    class: "check_fail".into(),
                    case: "alice.invite.from.userInfo".into(),
                    count: 3,
                    ok: false,
                    judged: Some(JudgedShare {
                        failed: 3,
                        judged: 10,
                        base: 50,
                        basis: JudgedBasis::Checked,
                    }),
                    error: Some("case checks: alice invite.from.userInfo: mismatch".into()),
                },
                CountRow {
                    scenario: "reinvite".into(),
                    class: "timeout".into(),
                    case: "bob@connected".into(),
                    count: 2,
                    ok: false,
                    judged: None,
                    error: None,
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
            rtt: vec![RttRow {
                scenario: "basic_call".into(),
                exchange: "invite_100".into(),
                n: 1185,
                p50_ms: 1.0,
                p90_ms: 2.0,
                p99_ms: 4.0,
            }],
            checks: vec![CheckSummaryRow { scenario: "reinvite".into(), passed: 7, failed: 3 }],
            canaries: Canaries {
                orphans: 0,
                shed: 4,
                drops_in: 6,
                drops_out: 5,
                inbox_drops: 0,
                ringing_expected: 1185,
                ringing_received: 1184,
            },
            samples: vec![SampleGroup {
                scenario: "reinvite".into(),
                class: "check_fail".into(),
                case: "alice.invite.from.userInfo".into(),
                pages: vec![
                    "callflows/reinvite/check_fail/alice.invite.from.userInfo/0.html".into()
                ],
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

    /// The derived tallies (total / failed) sum the buckets right.
    #[test]
    fn derived_totals() {
        let idx = sample_index();
        assert_eq!(idx.total_calls(), 1185);
        assert_eq!(idx.failed_calls(), 5, "3 check_fail + 2 timeout");
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
