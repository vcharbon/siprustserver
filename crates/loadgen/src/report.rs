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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use e2e_model::{
    Canaries, CheckSummaryRow, CheckpointRow, CountRow, LatencyRow, LoadRunIndex, LoadRunMeta,
    SampleGroup,
};

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

// (scenario, class label, case, chaos near/clear) — the chaos dimension
// auto-splits a failure class into kill-collateral (`near`) vs genuine
// (`clear`) sub-buckets; the case dimension ([`CallOutcome::case`]) keeps
// distinct failure modes of one class apart (which RFC rule, which failed
// check, which agent/phase), so the first-N sample capture holds N of EACH
// case instead of N copies of whichever error appeared first. Empty case =
// un-refined (Ok).
type Bucket = (ScenarioId, String, String, ChaosTag);

/// The run-dir-relative path of one sample page — THE single place the on-disk
/// layout is spelled, used by both `finalize` (the writer) and `build_index`
/// (the machine-readable listing) so the two can never disagree. An empty case
/// keeps the historic `scenario/class/chaos` layout.
fn sample_rel(scenario: &str, class: &str, case: &str, chaos: &str, i: usize) -> String {
    if case.is_empty() {
        format!("callflows/{scenario}/{class}/{chaos}/{i}.html")
    } else {
        format!("callflows/{scenario}/{class}/{case}/{chaos}/{i}.html")
    }
}

#[derive(Default)]
struct Inner {
    counts: BTreeMap<Bucket, u64>,
    shed: BTreeMap<ScenarioId, u64>,
    e2e: BTreeMap<ScenarioId, Hist>,
    checkpoints: BTreeMap<(ScenarioId, &'static str), Hist>,
    sample_taken: BTreeMap<Bucket, u32>,
    samples: BTreeMap<Bucket, Vec<RenderedSample>>,
    /// Per-scenario Test-case check-verdict tally over the SAMPLED calls:
    /// `(passed_all, failed_any)`. Fed only for calls whose attached case had
    /// checks that were actually evaluated (sampled OK/check-fail calls).
    check_verdicts: BTreeMap<ScenarioId, (u64, u64)>,
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

    /// Fold one SAMPLED call's Test-case check verdict into the per-scenario
    /// check-verdict tally: `all_passed` true bumps the passed count, false the
    /// failed count. Call only when the attached case's checks were actually
    /// evaluated (so an unsampled or case-less call never skews the tally).
    pub fn record_checks(&self, scenario: ScenarioId, all_passed: bool) {
        let mut g = self.inner.lock().unwrap();
        let e = g.check_verdicts.entry(scenario).or_default();
        if all_passed {
            e.0 += 1;
        } else {
            e.1 += 1;
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
        for ((s, _, _, _), &c) in g.sample_taken.iter() {
            if *s == scenario {
                saw_bucket = true;
                if c < self.cfg.sample_cap {
                    return true;
                }
            }
        }
        !saw_bucket // nothing recorded for this scenario yet → record
    }

    /// Whether a sample is still wanted for `(scenario, class, case, chaos)`
    /// (bucket < cap) — a cheap pre-check so the caller can skip the expensive
    /// HTML render when the bucket is already full. The case and chaos
    /// sub-buckets are independent, so each distinct failure mode — and its
    /// near/clear split — gets up to `sample_cap` flows.
    pub fn wants_sample(
        &self,
        scenario: ScenarioId,
        class: &ResultClass,
        case: &str,
        chaos: ChaosTag,
    ) -> bool {
        let g = self.inner.lock().unwrap();
        let key = (scenario, class.label(), case.to_string(), chaos);
        g.sample_taken.get(&key).copied().unwrap_or(0) < self.cfg.sample_cap
    }

    /// Record one completed call: bump its `(scenario, class, case)` count, fold
    /// its end-to-end time and checkpoints into the histograms, and store its
    /// sample if the bucket still has room. `case` is the outcome's bounded
    /// discriminator ([`CallOutcome::case`] — the driver computes it once, with
    /// the call's phase trail).
    pub fn record(
        &self,
        scenario: ScenarioId,
        outcome: &CallOutcome,
        case: &str,
        e2e: Duration,
        checkpoints: &[(&'static str, Duration)],
        sample: Option<RenderedSample>,
        chaos: ChaosTag,
    ) {
        let class = ResultClass::from(outcome);
        let label = class.label();
        let mut g = self.inner.lock().unwrap();
        *g.counts.entry((scenario, label.clone(), case.to_string(), chaos)).or_default() += 1;
        g.e2e.entry(scenario).or_insert_with(Hist::new).record(e2e.as_secs_f64() * 1000.0);
        for (name, d) in checkpoints {
            g.checkpoints
                .entry((scenario, name))
                .or_insert_with(Hist::new)
                .record(d.as_secs_f64() * 1000.0);
        }
        if let Some(sample) = sample {
            let key = (scenario, label, case.to_string(), chaos);
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
        // The case dimension is a report/sample refinement only — the Prometheus
        // series stay (scenario, class, chaos)-keyed (stable dashboards, bounded
        // series), so aggregate over case here.
        let mut prom_counts: BTreeMap<(ScenarioId, &String, ChaosTag), u64> = BTreeMap::new();
        for ((scenario, class, _case, chaos), n) in &g.counts {
            *prom_counts.entry((*scenario, class, *chaos)).or_default() += n;
        }
        for ((scenario, class, chaos), n) in &prom_counts {
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

        // Write each sample's callflow HTML, split by case (which failure mode)
        // and chaos sub-bucket (kill-collateral `near` apart from genuine `clear`).
        let mut links: BTreeMap<Bucket, Vec<String>> = BTreeMap::new();
        for ((scenario, class, case, chaos), samples) in &g.samples {
            let chaos_label = chaos.label();
            for (i, s) in samples.iter().enumerate() {
                let rel = sample_rel(scenario, class, case, chaos_label, i);
                let page = out_dir.join(&rel);
                if let Some(parent) = page.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if let Some(html) = &s.html {
                    std::fs::write(&page, html)?;
                } else {
                    // No rendered flow (sample taken on a non-recording call) —
                    // emit a stub so the detail is still linked.
                    let stub = format!(
                        "<html><body><h3>{scenario} / {class} / {case} / {chaos_label} #{i}</h3><p>{}</p>\
                         <p>e2e: {:.1} ms</p></body></html>",
                        s.detail.as_deref().unwrap_or("(no detail)"),
                        s.e2e_ms
                    );
                    std::fs::write(&page, stub)?;
                }
                links
                    .entry((scenario, class.clone(), case.clone(), *chaos))
                    .or_default()
                    .push(rel);
            }
        }

        // index.html
        let mut idx = String::new();
        idx.push_str("<html><head><meta charset=\"utf-8\"><title>loadgen report</title>\
            <style>body{font-family:sans-serif}table{border-collapse:collapse}\
            td,th{border:1px solid #ccc;padding:4px 8px}.ok{color:#070}.nok{color:#a00}\
            .near{color:#a60;font-weight:bold}</style>\
            </head><body><h1>loadgen report</h1>");
        idx.push_str("<h2>Results by scenario × class × case × chaos</h2>\
            <p>chaos=<b>clear</b> are the genuine results to triage; chaos=<b>near</b> are \
            within the chaos tolerance of an injected fault (likely acceptable kill collateral). \
            The <b>case</b> column splits a class into its distinct failure modes (RFC rule id, \
            failed check, agent@phase) — each keeps its own first-N samples.</p>\
            <table>\
            <tr><th>scenario</th><th>class</th><th>case</th><th>chaos</th><th>count</th><th>samples</th></tr>");
        for ((scenario, class, case, chaos), n) in &g.counts {
            let cls = if class == "ok" { "ok" } else { "nok" };
            let chaos_label = chaos.label();
            let chaos_cls = if chaos_label == "near" { "near" } else { "" };
            let sample_links = links
                .get(&(scenario, class.clone(), case.clone(), *chaos))
                .map(|ls| {
                    ls.iter()
                        .enumerate()
                        .map(|(i, l)| format!("<a href=\"{l}\">{i}</a>"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            idx.push_str(&format!(
                "<tr><td>{scenario}</td><td class=\"{cls}\">{class}</td><td>{case}</td>\
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
        writeln!(md, "## Results by scenario × class × case × chaos\n")?;
        writeln!(md, "(chaos=clear → genuine results to triage; chaos=near → within tolerance of an injected fault; case = the class's distinct failure mode)\n")?;
        writeln!(md, "| scenario | class | case | chaos | count |")?;
        writeln!(md, "|---|---|---|---|---|")?;
        for ((scenario, class, case, chaos), n) in &g.counts {
            writeln!(md, "| {scenario} | {class} | {case} | {} | {n} |", chaos.label())?;
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

    // -- Machine-readable index (load-result.json) -------------------------

    /// Build the [`LoadRunIndex`] — the machine-readable projection of the
    /// current reporter state — folding in the caller-supplied run `meta` (timing
    /// and echoed knobs, which the reporter is clock- and config-free about) and
    /// the external `canaries` (mux orphans/drops the reporter doesn't own). The
    /// sample-page paths it lists are EXACTLY the run-dir-relative paths that
    /// [`finalize`](Self::finalize) writes, so the two never disagree.
    pub fn build_index(&self, meta: LoadRunMeta, mut canaries: Canaries) -> LoadRunIndex {
        let g = self.inner.lock().unwrap();

        let counts = g
            .counts
            .iter()
            .map(|((scenario, class, case, chaos), n)| CountRow {
                scenario: scenario.to_string(),
                class: class.clone(),
                case: case.clone(),
                chaos: chaos.label().to_string(),
                count: *n,
                ok: class == "ok",
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

        let checks = g
            .check_verdicts
            .iter()
            .map(|(scenario, (passed, failed))| CheckSummaryRow {
                scenario: scenario.to_string(),
                passed: *passed,
                failed: *failed,
            })
            .collect();

        // The stored-sample links, keyed by their `(scenario, class, case,
        // chaos)` bucket — the SAME layout `finalize` writes to disk
        // (`sample_rel` is the single path authority).
        let samples = g
            .samples
            .iter()
            .map(|((scenario, class, case, chaos), stored)| {
                let chaos_label = chaos.label();
                let pages = (0..stored.len())
                    .map(|i| sample_rel(scenario, class, case, chaos_label, i))
                    .collect();
                SampleGroup {
                    scenario: scenario.to_string(),
                    class: class.clone(),
                    case: case.clone(),
                    chaos: chaos_label.to_string(),
                    pages,
                }
            })
            .collect();

        // The reporter owns shed + the 18x ringing gate; the caller supplies the
        // mux-owned orphans/drops. Fill the reporter half here so a caller can pass
        // `Canaries::default()` and still get correct shed/ringing.
        canaries.shed = g.shed.values().sum();
        canaries.ringing_expected = self.ringing_expected.load(Ordering::Relaxed);
        canaries.ringing_received = self.ringing_received.load(Ordering::Relaxed);

        LoadRunIndex { meta, counts, latency, checkpoints, checks, canaries, samples }
    }

    /// Write `load-result.json` (the [`LoadRunIndex`]) into `out_dir`. Called on
    /// every periodic report rewrite and at run end, right after
    /// [`finalize`](Self::finalize), so the machine-readable index sits next to
    /// `index.html` and lists the same sample pages that were just written.
    pub fn write_index(
        &self,
        out_dir: &Path,
        meta: LoadRunMeta,
        canaries: Canaries,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(out_dir)?;
        let index = self.build_index(meta, canaries);
        let json = serde_json::to_string_pretty(&index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(out_dir.join("load-result.json"), json + "\n")
    }

    /// One-shot: write the HTML/markdown report AND the machine-readable
    /// `load-result.json` for one snapshot (periodic or final). The bin calls this
    /// so both artifacts stay in lockstep.
    pub fn finalize_run(
        &self,
        out_dir: &Path,
        meta: LoadRunMeta,
        canaries: Canaries,
    ) -> std::io::Result<()> {
        self.finalize(out_dir)?;
        self.write_index(out_dir, meta, canaries)
    }

    /// Spawn the periodic on-disk snapshot task: every `every` it rewrites the
    /// HTML/markdown report AND `load-result.json` (via
    /// [`finalize_run`](Self::finalize_run)) so the report is browsable mid-run.
    /// `meta` must stamp `finished: false` — a snapshot is never the final word.
    ///
    /// SHUTDOWN ORDERING: the caller MUST `abort()` the returned handle and
    /// await it BEFORE the final `finalize_run(finished: true)` write. The task
    /// loops forever; left running, a tick that lands on the run's last instant
    /// (any report interval that divides the duration) races the final write and
    /// strands `finished: false` on disk (the 2026-07-03 validation finding).
    /// Awaiting the aborted handle guarantees no snapshot write is in flight or
    /// pending when the final write starts.
    pub fn spawn_snapshots(
        self: &Arc<Self>,
        out_dir: PathBuf,
        every: Duration,
        meta: impl Fn() -> LoadRunMeta + Send + 'static,
        canaries: impl Fn() -> Canaries + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let reporter = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(e) = reporter.finalize_run(&out_dir, meta(), canaries()) {
                    eprintln!("[loadgen] periodic report snapshot failed: {e}");
                }
            }
        })
    }

    // -- Test/inspection accessors -----------------------------------------

    /// Total count for a `(scenario, class)` bucket, summed across all case and
    /// chaos sub-buckets (for tests / summaries).
    pub fn count(&self, scenario: ScenarioId, class: &ResultClass) -> u64 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .counts
            .iter()
            .filter(|((s, c, _, _), _)| *s == scenario && *c == label)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Count for a `(scenario, class, chaos)` sub-bucket — e.g. the genuine
    /// (`Clear`) failures, isolated from kill collateral — summed across the
    /// class's case sub-buckets.
    pub fn count_tagged(&self, scenario: ScenarioId, class: &ResultClass, chaos: ChaosTag) -> u64 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .counts
            .iter()
            .filter(|((s, c, _, ch), _)| *s == scenario && *c == label && *ch == chaos)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Number of stored samples for a `(scenario, class)` bucket, across all
    /// case and chaos sub-buckets.
    pub fn sample_count(&self, scenario: ScenarioId, class: &ResultClass) -> u32 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .sample_taken
            .iter()
            .filter(|((s, c, _, _), _)| *s == scenario && *c == label)
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

    fn rfc_fail(rule: &str, detail: &str) -> CallOutcome {
        CallOutcome::RfcAuditFail(vec![sip_net::RfcFinding {
            rule: rule.to_string(),
            lane: "10.0.0.1:5060".to_string(),
            detail: detail.to_string(),
            advisory: false,
        }])
    }

    /// The chaos dimension splits a failure class into independent near/clear
    /// sub-buckets across counts, the Prometheus surface, and the on-disk dirs;
    /// the un-tagged `count` still sums both (so existing summaries are intact).
    #[test]
    fn chaos_splits_counts_samples_and_dirs() {
        let r = Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 0 });
        let mk = |d: &str| {
            Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 1.0 })
        };
        let outcome = rfc_fail("rfc3261.cseq", "CSeq went backwards");
        let case = outcome.case(None);
        r.record(
            "reinvite",
            &outcome,
            &case,
            Duration::from_millis(1),
            &[],
            mk("kill collateral"),
            ChaosTag::Near,
        );
        r.record(
            "reinvite",
            &outcome,
            &case,
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
            out.join("callflows/reinvite/rfc_audit_fail/rfc3261.cseq/near/0.html").exists(),
            "near sub-bucket dir missing"
        );
        assert!(
            out.join("callflows/reinvite/rfc_audit_fail/rfc3261.cseq/clear/0.html").exists(),
            "clear sub-bucket dir missing"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The case dimension splits ONE class into per-failure-mode sample buckets:
    /// two different RFC rules on the same scenario each keep their own first-N
    /// samples (the whole point — N samples of the first rule to fire no longer
    /// starve a second, rarer rule), and the counts table carries the case.
    #[test]
    fn case_splits_sample_buckets_within_a_class() {
        let r = Reporter::new(ReporterCfg { sample_cap: 1, background_record_every: 0 });
        let mk = |d: &str| {
            Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 1.0 })
        };
        let unacked = rfc_fail("rfc3261.unackedInviteNon2xxFinal", "reject never ACKed");
        let cseq = rfc_fail("rfc3261.cseq", "CSeq went backwards");
        let (case_a, case_b) = (unacked.case(None), cseq.case(None));
        assert_ne!(case_a, case_b, "different rules → different cases");

        // Fill rule A's bucket (cap 1), then rule B must STILL want a sample.
        r.record("reinvite", &unacked, &case_a, Duration::from_millis(1), &[], mk("a"), ChaosTag::Clear);
        assert!(
            !r.wants_sample("reinvite", &ResultClass::RfcAuditFail, &case_a, ChaosTag::Clear),
            "rule A bucket full at cap"
        );
        assert!(
            r.wants_sample("reinvite", &ResultClass::RfcAuditFail, &case_b, ChaosTag::Clear),
            "rule B keeps its own bucket"
        );
        r.record("reinvite", &cseq, &case_b, Duration::from_millis(1), &[], mk("b"), ChaosTag::Clear);
        assert_eq!(r.sample_count("reinvite", &ResultClass::RfcAuditFail), 2);

        // The machine-readable index carries the case on counts AND samples,
        // and the per-case pages land in per-case dirs.
        let out = std::env::temp_dir().join(format!("loadgen-case-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        r.finalize(&out).unwrap();
        let idx = r.build_index(
            LoadRunMeta {
                started_ms: 0,
                finished_ms: 0,
                finished: true,
                target: "t".into(),
                cps: 1.0,
                duration_secs: 1,
                max_in_flight: 1,
                egress: None,
                profile: None,
            },
            Canaries::default(),
        );
        assert!(idx.counts.iter().any(|c| c.case == case_a && c.count == 1));
        assert!(idx.counts.iter().any(|c| c.case == case_b && c.count == 1));
        for group in &idx.samples {
            for page in &group.pages {
                assert!(out.join(page).exists(), "index names a missing page: {page}");
                assert!(page.contains(&group.case), "page path carries the case: {page}");
            }
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    /// `finalize_run` writes `load-result.json` next to `index.html`; it parses
    /// back as a `LoadRunIndex`, its counts/canaries/check-verdicts mirror the
    /// reporter state, and every sample page it names actually exists on disk (the
    /// index and the HTML report never disagree).
    #[test]
    fn writes_and_parses_the_machine_readable_index() {
        let r = Reporter::new(ReporterCfg { sample_cap: 5, background_record_every: 0 });
        let mk = |d: &str| Some(RenderedSample { html: None, detail: Some(d.to_string()), e2e_ms: 2.0 });

        // One OK basic call, one genuine check-fail reinvite (sampled), plus the
        // cross-call ringing gate + a sampled check tally.
        r.record("basic_call", &CallOutcome::Ok, "", Duration::from_millis(3), &[("ringing", Duration::from_millis(1))], mk("ok"), ChaosTag::Clear);
        let check_fail = CallOutcome::CheckFail(vec![e2e_model::CheckVerdict {
            on: "alice.invite".to_string(),
            field: "from.userInfo".to_string(),
            op: e2e_model::CheckOp::Exists,
            expected: None,
            actual: None,
            passed: false,
            detail: "from.userInfo mismatch".to_string(),
        }]);
        let check_case = check_fail.case(None);
        r.record("reinvite", &check_fail, &check_case, Duration::from_millis(5), &[], mk("check fail"), ChaosTag::Clear);
        r.record_ringing(Some(true));
        r.record_ringing(Some(false));
        r.record_checks("reinvite", true);
        r.record_checks("reinvite", false);
        r.inc_shed("basic_call");

        let out = std::env::temp_dir().join(format!("loadgen-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let meta = LoadRunMeta {
            started_ms: 1_000,
            finished_ms: 61_000,
            finished: true,
            target: "10.0.0.1:5060".to_string(),
            cps: 20.0,
            duration_secs: 60,
            max_in_flight: 100,
            egress: Some("transparent".to_string()),
            profile: Some("smoke".to_string()),
        };
        let canaries = Canaries { orphans: 0, drops: 7, ..Canaries::default() };
        r.finalize_run(&out, meta.clone(), canaries).unwrap();

        let path = out.join("load-result.json");
        assert!(path.exists(), "load-result.json missing next to index.html");
        assert!(out.join("index.html").exists(), "index.html still written");
        let idx: LoadRunIndex =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(idx.meta, meta, "run metadata echoed verbatim");
        assert_eq!(idx.total_calls(), 2);
        assert_eq!(idx.failed_calls(), 1, "the check_fail reinvite");
        assert!(idx.counts.iter().any(|c| c.scenario == "basic_call" && c.class == "ok" && c.ok));
        assert!(idx.counts.iter().any(|c| c.scenario == "reinvite" && c.class == "check_fail" && !c.ok));

        // Canaries: reporter-owned shed/ringing filled in; caller-owned drops kept.
        assert_eq!(idx.canaries.shed, 1);
        assert_eq!(idx.canaries.drops, 7);
        assert_eq!((idx.canaries.ringing_received, idx.canaries.ringing_expected), (1, 2));

        // Per-scenario check-verdict tally.
        let cs = idx.checks.iter().find(|c| c.scenario == "reinvite").expect("reinvite check row");
        assert_eq!((cs.passed, cs.failed), (1, 1));

        // Every listed sample page exists on disk (the paths match what finalize wrote).
        assert!(!idx.samples.is_empty(), "at least one sample group");
        for group in &idx.samples {
            for page in &group.pages {
                assert!(out.join(page).exists(), "index names a missing sample page: {page}");
            }
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Regression (2026-07-03 validation finding): the periodic snapshot task
    /// races the final `finished:true` write whenever the report interval
    /// divides the run duration — a tick fired at the run's last instant
    /// overwrote the final index with a `finished:false` snapshot. The shutdown
    /// sequence must abort+await the task BEFORE the final write; after that,
    /// no tick may ever land again.
    #[tokio::test(start_paused = true)]
    async fn final_write_is_not_overwritten_by_snapshot_task() {
        let r = Arc::new(Reporter::new(ReporterCfg { sample_cap: 1, background_record_every: 0 }));
        r.record(
            "basic_call",
            &CallOutcome::Ok,
            "",
            Duration::from_millis(3),
            &[],
            None,
            ChaosTag::Clear,
        );

        let out =
            std::env::temp_dir().join(format!("loadgen-snapshot-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let base = LoadRunMeta {
            started_ms: 0,
            finished_ms: 0,
            finished: false,
            target: "10.0.0.1:5060".to_string(),
            cps: 1.0,
            duration_secs: 60,
            max_in_flight: 1,
            egress: None,
            profile: None,
        };
        let read_finished = |out: &Path| -> bool {
            let idx: LoadRunIndex = serde_json::from_str(
                &std::fs::read_to_string(out.join("load-result.json")).unwrap(),
            )
            .unwrap();
            idx.meta.finished
        };

        let snap_meta = base.clone();
        let snap = r.spawn_snapshots(
            out.clone(),
            Duration::from_secs(1),
            move || snap_meta.clone(),
            Canaries::default,
        );

        // Let a few ticks land: mid-run snapshots say finished:false.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(!read_finished(&out), "mid-run snapshots are finished:false");

        // The shutdown sequence under test: stop the task, THEN write the final.
        snap.abort();
        let _ = snap.await;
        r.finalize_run(&out, LoadRunMeta { finished: true, ..base }, Canaries::default())
            .unwrap();
        assert!(read_finished(&out), "final write says finished:true");

        // The regression: advance past several more would-be ticks — a leaked
        // task would overwrite the final index with a finished:false snapshot.
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(read_finished(&out), "no snapshot tick may land after shutdown");
        let _ = std::fs::remove_dir_all(&out);
    }
}
