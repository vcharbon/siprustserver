//! The load run index of the reporter's state: the one document every report
//! page is rendered from.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use e2e_model::{
    Canaries, CheckSummaryRow, CheckpointRow, CountRow, JudgedBasis, JudgedShare, LatencyRow,
    LoadRunIndex, LoadRunMeta, RttRow, SampleGroup, Sampling, ScenarioRecording,
};

use super::{sample_rel, Bucket, Inner, Reporter, CHASE_CALLS};
use crate::class::ResultClass;
use crate::scenarios::ScenarioId;

impl Reporter {
    /// Build the [`LoadRunIndex`] of the current reporter state, folding in
    /// the caller-supplied run `meta` (timing and echoed knobs; the reporter
    /// sets its sampling bounds) and the external `canaries` (the mux's
    /// orphans and drops; the reporter fills shed and ringing). Its progress
    /// holds the reporter's figures, with the configured rate as target and
    /// the offering time cut at the configured duration; the caller fills the
    /// rate, drain and call-outcome figures it owns. The sample-page
    /// paths it lists are the run-dir-relative paths
    /// [`write_report`](Self::write_report) writes and the live surface serves.
    pub fn build_index(&self, mut meta: LoadRunMeta, mut canaries: Canaries) -> LoadRunIndex {
        let g = self.inner.lock().unwrap();
        meta.sampling = Some(Sampling {
            sample_cap: self.cfg.sample_cap,
            record_every: self.cfg.background_record_every,
            chase_calls: CHASE_CALLS,
        });
        let mut progress = self.progress_of(&g);
        progress.target_cps = meta.cps;
        if meta.duration_secs > 0 {
            progress.offering_ms = progress.elapsed_ms.min(meta.duration_secs * 1000);
        }

        let counts = g
            .counts
            .iter()
            .map(|(bucket, n)| {
                let (scenario, class, case) = bucket;
                CountRow {
                    scenario: scenario.to_string(),
                    class: class.clone(),
                    case: case.clone(),
                    count: *n,
                    ok: ResultClass::label_is_ok(class),
                    judged: judged_share(&g, bucket, *n),
                    error: g
                        .samples
                        .get(bucket)
                        .and_then(|stored| stored.iter().find_map(|s| s.detail.clone())),
                }
            })
            .collect();

        let latency = g
            .e2e
            .iter()
            .map(|(scenario, h)| LatencyRow {
                scenario: scenario.to_string(),
                n: h.total,
                mean_ms: h.mean_ms(),
                p50_ms: h.quantile_ms(0.5),
                p90_ms: h.quantile_ms(0.9),
                p99_ms: h.quantile_ms(0.99),
                max_ms: h.max,
            })
            .collect();

        let checkpoints = g
            .checkpoints
            .iter()
            .map(|((scenario, name), h)| CheckpointRow {
                scenario: scenario.to_string(),
                checkpoint: name.to_string(),
                n: h.total,
                p50_ms: h.quantile_ms(0.5),
                p90_ms: h.quantile_ms(0.9),
                p99_ms: h.quantile_ms(0.99),
            })
            .collect();

        let rtt = g
            .rtt
            .iter()
            .map(|((scenario, exchange), h)| RttRow {
                scenario: scenario.to_string(),
                exchange: exchange.label().to_string(),
                n: h.total,
                p50_ms: h.quantile_ms(0.5),
                p90_ms: h.quantile_ms(0.9),
                p99_ms: h.quantile_ms(0.99),
            })
            .collect();

        let checks = g
            .check_verdicts
            .iter()
            .map(|(scenario, (passed, failed))| CheckSummaryRow {
                scenario: scenario.to_string(),
                passed: *passed,
                failed: *failed,
            })
            .collect();

        let samples = g
            .samples
            .iter()
            .map(|((scenario, class, case), stored)| SampleGroup {
                scenario: scenario.to_string(),
                class: class.clone(),
                case: case.clone(),
                pages: (0..stored.len()).map(|i| sample_rel(scenario, class, case, i)).collect(),
            })
            .collect();

        let tally = |m: &BTreeMap<ScenarioId, u64>, s: &ScenarioId| m.get(s).copied().unwrap_or(0);
        let recording = g
            .placed
            .iter()
            .map(|(scenario, placed)| ScenarioRecording {
                scenario: scenario.to_string(),
                placed: *placed,
                recorded: tally(&g.recorded, scenario),
                accepted: tally(&g.accepted, scenario),
                audited: tally(&g.audited, scenario),
            })
            .collect();

        canaries.shed = g.shed.values().sum();
        canaries.ringing_expected = self.ringing_expected.load(Ordering::Relaxed);
        canaries.ringing_received = self.ringing_received.load(Ordering::Relaxed);

        LoadRunIndex {
            meta,
            progress,
            series: g.live.series.points(),
            rate_events: Vec::new(),
            rate_events_dropped: 0,
            recent_failures: g.live.failures.iter().cloned().collect(),
            recording,
            counts,
            latency,
            checkpoints,
            rtt,
            checks,
            canaries,
            samples,
        }
    }
}

/// A row's share of the recorded calls its class judges on their recording,
/// and the estimate over every call that class judges: an `rfc_audit_fail`
/// row counts its audited recorded calls over the audited ones, estimated
/// over the calls that ended on an accepted branch; a `check_fail` row counts
/// its calls over the checked ones, estimated over the accepted calls clean of
/// the audit. Every other row is judged on every call and has no share, and
/// so has an `rfc_audit_fail` row with no audited recorded call.
fn judged_share(g: &Inner, bucket: &Bucket, n: u64) -> Option<JudgedShare> {
    let (scenario, class, _) = bucket;
    let tally = |m: &BTreeMap<ScenarioId, u64>| m.get(scenario).copied().unwrap_or(0);
    let (failed, judged, base, basis) = if *class == ResultClass::RfcAuditFail.label() {
        let k = g.audited_failed.get(bucket).copied().unwrap_or(0);
        (k, tally(&g.audited), tally(&g.accepted), JudgedBasis::Audited)
    } else if *class == ResultClass::CheckFail.label() {
        let (passed, failed) = g.check_verdicts.get(scenario).copied().unwrap_or((0, 0));
        let count = |label: String| -> u64 {
            g.counts
                .iter()
                .filter(|((s, c, _), _)| s == scenario && *c == label)
                .map(|(_, n)| n)
                .sum()
        };
        let base = count(ResultClass::Ok.label()) + count(ResultClass::CheckFail.label());
        (n, passed + failed, base, JudgedBasis::Checked)
    } else {
        return None;
    };
    (failed > 0 && judged > 0).then_some(JudgedShare { failed, judged, base, basis })
}
