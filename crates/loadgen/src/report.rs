//! Bounded-memory reporting: per-`(scenario, class)` counters, end-to-end and
//! named-checkpoint latency histograms, a bounded sample store (the first N
//! callflows per `(scenario, class)` — **including OK**, so OK vs failing flows
//! are comparable), a live Prometheus `/metrics` surface, and a final on-disk
//! HTML/markdown report.
//!
//! The memory ceiling is structural: counters and histograms are fixed-size;
//! samples are capped per bucket; and the [`SamplingGate`] decides at call start
//! whether to record at all, converging to ~zero recording once every bucket is
//! full (with a small background fraction so a late-appearing error class still
//! gets captured). Recording itself is per-call (freed when the call's binder
//! drops), so nothing accumulates across calls.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::chaos::ChaosTag;
use crate::class::{CallOutcome, ResultClass};
use crate::scenarios::ScenarioId;

// ---------------------------------------------------------------------------
// Fixed-bucket latency histogram (no external dep)
// ---------------------------------------------------------------------------

/// A small fixed log-bucket histogram (≈0.1 ms … ~700 s, 48 buckets). Bounded
/// memory, O(1) record, approximate quantiles (bucket upper bound). Good enough
/// for load-test p50/p90/p99; avoids a new crate dependency.
#[derive(Clone)]
pub struct Hist {
    bounds: Vec<f64>,
    counts: Vec<u64>,
    total: u64,
    sum: f64,
    max: f64,
}

impl Hist {
    fn new() -> Self {
        let bounds: Vec<f64> = (0..48).map(|i| 0.1 * 1.4f64.powi(i)).collect();
        let counts = vec![0u64; bounds.len() + 1];
        Self { bounds, counts, total: 0, sum: 0.0, max: 0.0 }
    }

    fn record(&mut self, ms: f64) {
        let idx = self.bounds.partition_point(|b| *b < ms);
        self.counts[idx] += 1;
        self.total += 1;
        self.sum += ms;
        if ms > self.max {
            self.max = ms;
        }
    }

    /// Approximate quantile in milliseconds (the upper bound of the bucket the
    /// q-th value falls in; `max` for the overflow bucket).
    fn quantile_ms(&self, q: f64) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let target = (q * self.total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            cum += c;
            if cum >= target {
                return self.bounds.get(i).copied().unwrap_or(self.max).min(self.max.max(0.0)).max(0.0);
            }
        }
        self.max
    }

    fn mean_ms(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.sum / self.total as f64
        }
    }
}

// ---------------------------------------------------------------------------
// A rendered sample (a captured callflow for one (scenario, class) bucket)
// ---------------------------------------------------------------------------

/// One captured call: the rendered callflow HTML (if the call was sampled and
/// rendered) plus a one-line detail and its end-to-end time. Stored bounded
/// per `(scenario, class)`.
pub struct RenderedSample {
    pub html: Option<String>,
    pub detail: Option<String>,
    pub e2e_ms: f64,
}

// ---------------------------------------------------------------------------
// Reporter
// ---------------------------------------------------------------------------

/// Reporter config.
#[derive(Clone)]
pub struct ReporterCfg {
    /// Max stored callflow samples per `(scenario, class)` bucket.
    pub sample_cap: u32,
    /// Record roughly 1 call in `background_record_every` even once all buckets
    /// are full, so a late-appearing error class still gets a sample. 0 disables.
    pub background_record_every: u64,
}

impl Default for ReporterCfg {
    fn default() -> Self {
        Self { sample_cap: 10, background_record_every: 64 }
    }
}

// (scenario, class label, chaos near/clear) — the chaos dimension auto-splits a
// failure class into kill-collateral (`near`) vs genuine (`clear`) sub-buckets.
type Bucket = (ScenarioId, String, ChaosTag);

#[derive(Default)]
struct Inner {
    counts: BTreeMap<Bucket, u64>,
    shed: BTreeMap<ScenarioId, u64>,
    e2e: BTreeMap<ScenarioId, Hist>,
    checkpoints: BTreeMap<(ScenarioId, &'static str), Hist>,
    sample_taken: BTreeMap<Bucket, u32>,
    samples: BTreeMap<Bucket, Vec<RenderedSample>>,
}

/// The bounded-memory load-test reporter. Cloneable handle via `Arc`.
pub struct Reporter {
    cfg: ReporterCfg,
    inner: Mutex<Inner>,
    inflight: AtomicU64,
    started: AtomicU64,
    record_counter: AtomicU64,
    /// Calls that reached the ring→answer step (the 18x-delivery denominator).
    ringing_expected: AtomicU64,
    /// Of those, how many saw their `18x` ringing provisional (numerator). A
    /// dropped non-PRACK 18x is EXPECTED (not a failure), so this is tracked as a
    /// cross-call rate gated at >99% rather than failing the individual call.
    ringing_received: AtomicU64,
}

impl Reporter {
    pub fn new(cfg: ReporterCfg) -> Self {
        Self {
            cfg,
            inner: Mutex::new(Inner::default()),
            inflight: AtomicU64::new(0),
            started: AtomicU64::new(0),
            record_counter: AtomicU64::new(0),
            ringing_expected: AtomicU64::new(0),
            ringing_received: AtomicU64::new(0),
        }
    }

    /// Fold one call's ringing outcome into the cross-call 18x-delivery gate:
    /// `None` = the call never reached the answer step (not counted); `Some(true)`
    /// = the 18x arrived; `Some(false)` = it was legitimately lost.
    pub fn record_ringing(&self, ringing: Option<bool>) {
        if let Some(received) = ringing {
            self.ringing_expected.fetch_add(1, Ordering::Relaxed);
            if received {
                self.ringing_received.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// `(received, expected)` for the cross-call 18x-delivery gate.
    pub fn ringing_totals(&self) -> (u64, u64) {
        (
            self.ringing_received.load(Ordering::Relaxed),
            self.ringing_expected.load(Ordering::Relaxed),
        )
    }

    pub fn inc_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// A call was dropped at the max-in-flight cap (offered load we shed).
    pub fn inc_shed(&self, scenario: ScenarioId) {
        *self.inner.lock().unwrap().shed.entry(scenario).or_default() += 1;
    }

    /// The sampling-gate decision at call start: should this call record its
    /// trace? True while any bucket for the scenario is still under cap (or none
    /// recorded yet), plus a small background fraction so a late error class is
    /// eventually captured. Converges to (background-only) once full.
    pub fn should_record(&self, scenario: ScenarioId) -> bool {
        let n = self.record_counter.fetch_add(1, Ordering::Relaxed);
        if self.cfg.background_record_every != 0 && n.is_multiple_of(self.cfg.background_record_every) {
            return true;
        }
        let g = self.inner.lock().unwrap();
        let mut saw_bucket = false;
        for ((s, _, _), &c) in g.sample_taken.iter() {
            if *s == scenario {
                saw_bucket = true;
                if c < self.cfg.sample_cap {
                    return true;
                }
            }
        }
        !saw_bucket // nothing recorded for this scenario yet → record
    }

    /// Whether a sample is still wanted for `(scenario, class, chaos)` (bucket <
    /// cap) — a cheap pre-check so the caller can skip the expensive HTML render
    /// when the bucket is already full. The chaos sub-bucket is independent, so a
    /// near-chaos and a clear failure each get up to `sample_cap` flows.
    pub fn wants_sample(&self, scenario: ScenarioId, class: &ResultClass, chaos: ChaosTag) -> bool {
        let g = self.inner.lock().unwrap();
        let key = (scenario, class.label(), chaos);
        g.sample_taken.get(&key).copied().unwrap_or(0) < self.cfg.sample_cap
    }

    /// Record one completed call: bump its `(scenario, class)` count, fold its
    /// end-to-end time and checkpoints into the histograms, and store its sample
    /// if the bucket still has room.
    pub fn record(
        &self,
        scenario: ScenarioId,
        outcome: &CallOutcome,
        e2e: Duration,
        checkpoints: &[(&'static str, Duration)],
        sample: Option<RenderedSample>,
        chaos: ChaosTag,
    ) {
        let class = ResultClass::from(outcome);
        let label = class.label();
        let mut g = self.inner.lock().unwrap();
        *g.counts.entry((scenario, label.clone(), chaos)).or_default() += 1;
        g.e2e.entry(scenario).or_insert_with(Hist::new).record(e2e.as_secs_f64() * 1000.0);
        for (name, d) in checkpoints {
            g.checkpoints
                .entry((scenario, name))
                .or_insert_with(Hist::new)
                .record(d.as_secs_f64() * 1000.0);
        }
        if let Some(sample) = sample {
            let key = (scenario, label, chaos);
            let taken = g.sample_taken.entry(key.clone()).or_default();
            if *taken < self.cfg.sample_cap {
                *taken += 1;
                g.samples.entry(key).or_default().push(sample);
            }
        }
    }

    // -- Prometheus ---------------------------------------------------------

    /// Render the live `/metrics` text in Prometheus exposition format. Series
    /// mirror the SIPp exporter's naming so existing dashboards/queries extend.
    pub fn render_prometheus(&self) -> String {
        let g = self.inner.lock().unwrap();
        let mut out = String::new();
        out.push_str("# HELP loadgen_calls_total Completed load calls by scenario, result class, and chaos proximity.\n");
        out.push_str("# TYPE loadgen_calls_total counter\n");
        for ((scenario, class, chaos), n) in &g.counts {
            out.push_str(&format!(
                "loadgen_calls_total{{scenario=\"{scenario}\",class=\"{class}\",chaos=\"{}\"}} {n}\n",
                chaos.label()
            ));
        }
        out.push_str("# HELP loadgen_shed_total Calls dropped at the max-in-flight cap.\n");
        out.push_str("# TYPE loadgen_shed_total counter\n");
        for (scenario, n) in &g.shed {
            out.push_str(&format!("loadgen_shed_total{{scenario=\"{scenario}\"}} {n}\n"));
        }
        out.push_str("# HELP loadgen_inflight Calls currently in flight.\n");
        out.push_str("# TYPE loadgen_inflight gauge\n");
        out.push_str(&format!("loadgen_inflight {}\n", self.inflight.load(Ordering::Relaxed)));
        out.push_str("# HELP loadgen_started_total Calls started.\n");
        out.push_str("# TYPE loadgen_started_total counter\n");
        out.push_str(&format!("loadgen_started_total {}\n", self.started.load(Ordering::Relaxed)));
        // 18x-delivery gate: a non-PRACK ringing provisional is best-effort, so a
        // miss is EXPECTED. `received/expected` should stay > 0.99; a systemic 18x
        // regression drops it well below and IS a bug (unlike one dropped 18x).
        out.push_str("# HELP loadgen_ringing_expected_total Calls that reached the ring→answer step (18x denominator).\n");
        out.push_str("# TYPE loadgen_ringing_expected_total counter\n");
        out.push_str(&format!(
            "loadgen_ringing_expected_total {}\n",
            self.ringing_expected.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP loadgen_ringing_received_total Of those, calls whose caller received the 18x ringing provisional.\n");
        out.push_str("# TYPE loadgen_ringing_received_total counter\n");
        out.push_str(&format!(
            "loadgen_ringing_received_total {}\n",
            self.ringing_received.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP loadgen_e2e_seconds End-to-end call latency quantiles.\n");
        out.push_str("# TYPE loadgen_e2e_seconds gauge\n");
        for (scenario, h) in &g.e2e {
            for q in [0.5, 0.9, 0.99] {
                out.push_str(&format!(
                    "loadgen_e2e_seconds{{scenario=\"{scenario}\",quantile=\"{q}\"}} {:.6}\n",
                    h.quantile_ms(q) / 1000.0
                ));
            }
        }
        out.push_str("# HELP loadgen_checkpoint_seconds Named-checkpoint latency quantiles.\n");
        out.push_str("# TYPE loadgen_checkpoint_seconds gauge\n");
        for ((scenario, name), h) in &g.checkpoints {
            for q in [0.5, 0.9, 0.99] {
                out.push_str(&format!(
                    "loadgen_checkpoint_seconds{{scenario=\"{scenario}\",checkpoint=\"{name}\",quantile=\"{q}\"}} {:.6}\n",
                    h.quantile_ms(q) / 1000.0
                ));
            }
        }
        out
    }

    // -- Final on-disk report ----------------------------------------------

    /// Write the on-disk report under `out_dir`: per-`(scenario, class)`
    /// callflow HTML pages, an `index.html` (counts table + latency percentiles
    /// + links), and a `summary.md`.
    pub fn finalize(&self, out_dir: &Path) -> std::io::Result<()> {
        let g = self.inner.lock().unwrap();
        std::fs::create_dir_all(out_dir)?;
        let flows_root = out_dir.join("callflows");

        // Write each sample's callflow HTML, split by chaos sub-bucket so the
        // kill-collateral (`near`) flows sit apart from the genuine (`clear`) ones.
        let mut links: BTreeMap<Bucket, Vec<String>> = BTreeMap::new();
        for ((scenario, class, chaos), samples) in &g.samples {
            let chaos_label = chaos.label();
            let dir = flows_root.join(scenario).join(class).join(chaos_label);
            std::fs::create_dir_all(&dir)?;
            for (i, s) in samples.iter().enumerate() {
                let rel = format!("callflows/{scenario}/{class}/{chaos_label}/{i}.html");
                if let Some(html) = &s.html {
                    std::fs::write(out_dir.join(&rel), html)?;
                } else {
                    // No rendered flow (sample taken on a non-recording call) —
                    // emit a stub so the detail is still linked.
                    let stub = format!(
                        "<html><body><h3>{scenario} / {class} / {chaos_label} #{i}</h3><p>{}</p>\
                         <p>e2e: {:.1} ms</p></body></html>",
                        s.detail.as_deref().unwrap_or("(no detail)"),
                        s.e2e_ms
                    );
                    std::fs::write(out_dir.join(&rel), stub)?;
                }
                links.entry((scenario, class.clone(), *chaos)).or_default().push(rel);
            }
        }

        // index.html
        let mut idx = String::new();
        idx.push_str("<html><head><meta charset=\"utf-8\"><title>loadgen report</title>\
            <style>body{font-family:sans-serif}table{border-collapse:collapse}\
            td,th{border:1px solid #ccc;padding:4px 8px}.ok{color:#070}.nok{color:#a00}\
            .near{color:#a60;font-weight:bold}</style>\
            </head><body><h1>loadgen report</h1>");
        idx.push_str("<h2>Results by scenario × class × chaos</h2>\
            <p>chaos=<b>clear</b> are the genuine results to triage; chaos=<b>near</b> are \
            within the chaos tolerance of an injected fault (likely acceptable kill collateral).</p>\
            <table>\
            <tr><th>scenario</th><th>class</th><th>chaos</th><th>count</th><th>samples</th></tr>");
        for ((scenario, class, chaos), n) in &g.counts {
            let cls = if class == "ok" { "ok" } else { "nok" };
            let chaos_label = chaos.label();
            let chaos_cls = if chaos_label == "near" { "near" } else { "" };
            let sample_links = links
                .get(&(scenario, class.clone(), *chaos))
                .map(|ls| {
                    ls.iter()
                        .enumerate()
                        .map(|(i, l)| format!("<a href=\"{l}\">{i}</a>"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            idx.push_str(&format!(
                "<tr><td>{scenario}</td><td class=\"{cls}\">{class}</td>\
                 <td class=\"{chaos_cls}\">{chaos_label}</td><td>{n}</td><td>{sample_links}</td></tr>"
            ));
        }
        idx.push_str("</table><h2>Latency (ms)</h2><table>\
            <tr><th>scenario</th><th>n</th><th>mean</th><th>p50</th><th>p90</th><th>p99</th><th>max</th></tr>");
        for (scenario, h) in &g.e2e {
            idx.push_str(&format!(
                "<tr><td>{scenario}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
                h.total, h.mean_ms(), h.quantile_ms(0.5), h.quantile_ms(0.9), h.quantile_ms(0.99), h.max
            ));
        }
        idx.push_str("</table>");
        if !g.checkpoints.is_empty() {
            idx.push_str("<h2>Checkpoints (ms)</h2><table>\
                <tr><th>scenario</th><th>checkpoint</th><th>n</th><th>p50</th><th>p90</th><th>p99</th></tr>");
            for ((scenario, name), h) in &g.checkpoints {
                idx.push_str(&format!(
                    "<tr><td>{scenario}</td><td>{name}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
                    h.total, h.quantile_ms(0.5), h.quantile_ms(0.9), h.quantile_ms(0.99)
                ));
            }
            idx.push_str("</table>");
        }
        idx.push_str("</body></html>");
        std::fs::write(out_dir.join("index.html"), idx)?;

        // summary.md
        let mut md = std::fs::File::create(out_dir.join("summary.md"))?;
        writeln!(md, "# loadgen summary\n")?;
        writeln!(md, "## Results by scenario × class × chaos\n")?;
        writeln!(md, "(chaos=clear → genuine results to triage; chaos=near → within tolerance of an injected fault)\n")?;
        writeln!(md, "| scenario | class | chaos | count |")?;
        writeln!(md, "|---|---|---|---|")?;
        for ((scenario, class, chaos), n) in &g.counts {
            writeln!(md, "| {scenario} | {class} | {} | {n} |", chaos.label())?;
        }
        writeln!(md, "\n## Latency (ms)\n")?;
        writeln!(md, "| scenario | n | mean | p50 | p90 | p99 | max |")?;
        writeln!(md, "|---|---|---|---|---|---|---|")?;
        for (scenario, h) in &g.e2e {
            writeln!(
                md,
                "| {scenario} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                h.total, h.mean_ms(), h.quantile_ms(0.5), h.quantile_ms(0.9), h.quantile_ms(0.99), h.max
            )?;
        }
        Ok(())
    }

    // -- Test/inspection accessors -----------------------------------------

    /// Total count for a `(scenario, class)` bucket, summed across both chaos
    /// sub-buckets (for tests / summaries).
    pub fn count(&self, scenario: ScenarioId, class: &ResultClass) -> u64 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .counts
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Count for a specific `(scenario, class, chaos)` sub-bucket — e.g. the
    /// genuine (`Clear`) failures, isolated from kill collateral.
    pub fn count_tagged(&self, scenario: ScenarioId, class: &ResultClass, chaos: ChaosTag) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .counts
            .get(&(scenario, class.label(), chaos))
            .copied()
            .unwrap_or(0)
    }

    /// Number of stored samples for a `(scenario, class)` bucket, across both
    /// chaos sub-buckets.
    pub fn sample_count(&self, scenario: ScenarioId, class: &ResultClass) -> u32 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .sample_taken
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Total completed calls across all buckets.
    pub fn total_calls(&self) -> u64 {
        self.inner.lock().unwrap().counts.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chaos dimension splits a failure class into independent near/clear
    /// sub-buckets across counts, the Prometheus surface, and the on-disk dirs;
    /// the un-tagged `count` still sums both (so existing summaries are intact).
    #[test]
    fn chaos_splits_counts_samples_and_dirs() {
        let r = Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 0 });
        let mk = |d: &str| {
            Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 1.0 })
        };
        r.record(
            "reinvite",
            &CallOutcome::RfcAuditFail("kill collateral".into()),
            Duration::from_millis(1),
            &[],
            mk("kill collateral"),
            ChaosTag::Near,
        );
        r.record(
            "reinvite",
            &CallOutcome::RfcAuditFail("genuine desync".into()),
            Duration::from_millis(1),
            &[],
            mk("genuine desync"),
            ChaosTag::Clear,
        );

        assert_eq!(r.count_tagged("reinvite", &ResultClass::RfcAuditFail, ChaosTag::Near), 1);
        assert_eq!(r.count_tagged("reinvite", &ResultClass::RfcAuditFail, ChaosTag::Clear), 1);
        assert_eq!(r.count("reinvite", &ResultClass::RfcAuditFail), 2, "untagged count sums both");

        let prom = r.render_prometheus();
        assert!(prom.contains("class=\"rfc_audit_fail\",chaos=\"near\""), "{prom}");
        assert!(prom.contains("class=\"rfc_audit_fail\",chaos=\"clear\""), "{prom}");

        let out = std::env::temp_dir().join(format!("loadgen-chaos-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        r.finalize(&out).unwrap();
        assert!(
            out.join("callflows/reinvite/rfc_audit_fail/near/0.html").exists(),
            "near sub-bucket dir missing"
        );
        assert!(
            out.join("callflows/reinvite/rfc_audit_fail/clear/0.html").exists(),
            "clear sub-bucket dir missing"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
