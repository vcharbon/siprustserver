//! Bounded-memory reporting: per-`(scenario, class)` counters, per-scenario
//! accepted-branch and failed-step counters, end-to-end and
//! named-checkpoint latency histograms, per-`(scenario, exchange)` SIP
//! round-trip histograms, a bounded sample store (the first N
//! callflows per `(scenario, class)` — **including OK**, so OK vs failing flows
//! are comparable), a live Prometheus `/metrics` surface, and a final on-disk
//! HTML/markdown report.
//!
//! The memory ceiling is structural: counters and histograms are fixed-size;
//! samples are capped per bucket; and the sampling gate
//! ([`Reporter::should_record`]) decides at call start
//! whether to record at all, converging to ~zero recording once every bucket is
//! full or has chased its next sample long enough (with a small background
//! fraction so a late-appearing error class still gets captured). Recording itself is per-call (freed when the call's binder
//! drops), so nothing accumulates across calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use e2e_model::{
    Canaries, CheckSummaryRow, CheckpointRow, CountRow, LatencyRow, LoadRunIndex, LoadRunMeta,
    SampleGroup,
};

use crate::class::{CallOutcome, ResultClass};
use crate::hist::Hist;
use crate::mux::Exchange;
use crate::scenarios::ScenarioId;
use scenario_harness::actor::FailedAt;

// ---------------------------------------------------------------------------
// A rendered sample (a captured callflow for one (scenario, class) bucket)
// ---------------------------------------------------------------------------

/// The on-disk report rendered under the reporter's lock, written after it.
struct ReportFiles {
    /// Every sample page, by run-dir-relative path.
    pages: Vec<(String, Arc<str>)>,
    index: String,
    summary: String,
}

/// One captured call: the rendered callflow HTML (if the call was sampled and
/// rendered) plus a one-line detail and its end-to-end time. Stored bounded
/// per `(scenario, class)`; the page is shared, so a report snapshot takes it
/// without copying.
pub struct RenderedSample {
    pub html: Option<Arc<str>>,
    pub detail: Option<String>,
    pub e2e_ms: f64,
}

// ---------------------------------------------------------------------------
// Reporter
// ---------------------------------------------------------------------------

/// The sample bounds of a [`Reporter`].
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

// (scenario, class label, case) — the case dimension ([`CallOutcome::case`]) keeps
// distinct failure modes of one class apart (which RFC rule, which failed
// check, which agent/phase), so the first-N sample capture holds N of EACH
// case instead of N copies of whichever error appeared first. Empty case =
// un-refined (Ok).
type Bucket = (ScenarioId, String, String);

/// How many calls of its scenario a bucket under cap keeps recording after its
/// last sample: the sampling gate chases the bucket's next occurrence that
/// long, then leaves it to the background fraction, so a rare failure mode
/// never keeps a whole scenario recording.
const CHASE_CALLS: u32 = 64;

/// One sample bucket's tally: the samples it holds, and the calls recorded to
/// chase its next sample since the last one ([`CHASE_CALLS`]).
#[derive(Default, Clone, Copy)]
struct BucketTally {
    taken: u32,
    chased: u32,
}

/// The run-dir-relative path of one sample page — THE single place the on-disk
/// layout is spelled, used by both `finalize` (the writer) and `build_index`
/// (the machine-readable listing) so the two can never disagree. An empty case
/// uses the `scenario/class` layout.
fn sample_rel(scenario: &str, class: &str, case: &str, i: usize) -> String {
    if case.is_empty() {
        format!("callflows/{scenario}/{class}/{i}.html")
    } else {
        format!("callflows/{scenario}/{class}/{case}/{i}.html")
    }
}

#[derive(Default)]
struct Inner {
    /// The load stream every series names (`main` until set).
    stream: Option<String>,
    /// The run's mix, written from the first scrape.
    declared: std::collections::BTreeSet<ScenarioId>,
    counts: BTreeMap<Bucket, u64>,
    /// Calls classed ok, by the accepted branch they ended on.
    branches: BTreeMap<(ScenarioId, &'static str), u64>,
    /// Failed calls, by the step they failed at.
    failed_steps: BTreeMap<(ScenarioId, FailedAt), u64>,
    shed: BTreeMap<ScenarioId, u64>,
    e2e: BTreeMap<ScenarioId, Hist>,
    checkpoints: BTreeMap<(ScenarioId, &'static str), Hist>,
    /// SIP round trips per scenario and exchange (`Hist::round_trip` bounds).
    rtt: BTreeMap<(ScenarioId, Exchange), Hist>,
    sample_taken: BTreeMap<Bucket, BucketTally>,
    samples: BTreeMap<Bucket, Vec<RenderedSample>>,
    /// Per-scenario Test-case check-verdict tally over the SAMPLED calls:
    /// `(passed_all, failed_any)`. Fed only for calls whose attached case had
    /// checks that were actually evaluated (sampled OK/check-fail calls).
    check_verdicts: BTreeMap<ScenarioId, (u64, u64)>,
    /// Per-scenario structural-waiver campaign tally (ADR-0024 §6): a waiver's
    /// "was ever used" OR-aggregated across the scenario's sampled calls, so the
    /// unused-waiver gate is per CAMPAIGN, never per call.
    waivers: BTreeMap<ScenarioId, crate::waiver::CampaignWaivers>,
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

    /// Fold one sampled call's per-waiver used mask into its scenario's CAMPAIGN
    /// tally (ADR-0024 §6). `waivers` is the call's stable merged waiver list
    /// (case + plan); `used` is aligned to it. Idempotent to call on an
    /// unsampled call (its mask is all-false → a no-op).
    pub fn record_waiver_use(
        &self,
        scenario: ScenarioId,
        waivers: &[scenario_harness::WaiverScope],
        used: &[bool],
    ) {
        if waivers.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.waivers
            .entry(scenario)
            .or_insert_with(|| crate::waiver::CampaignWaivers::new(waivers.to_vec()))
            .record(used);
    }

    /// The stale non-conditional waivers per scenario at campaign end (unused all
    /// run long) — `(scenario, rule, justification)`, surfaced by the runner.
    pub fn unused_waivers(&self) -> Vec<(ScenarioId, String, String)> {
        let g = self.inner.lock().unwrap();
        g.waivers
            .iter()
            .flat_map(|(id, cw)| {
                cw.unused().into_iter().map(move |w| (*id, w.rule.clone(), w.justification.clone()))
            })
            .collect()
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
    /// trace? True while a bucket for the scenario is still under cap and has
    /// chased its next sample for fewer than `CHASE_CALLS` (64) calls (or none is
    /// recorded yet), plus a small background fraction so a late error class is
    /// eventually captured. Converges to (background-only) once full.
    pub fn should_record(&self, scenario: ScenarioId) -> bool {
        let n = self.record_counter.fetch_add(1, Ordering::Relaxed);
        if self.cfg.background_record_every != 0
            && n.is_multiple_of(self.cfg.background_record_every)
        {
            return true;
        }
        let mut g = self.inner.lock().unwrap();
        let mut saw_bucket = false;
        for ((s, _, _), tally) in g.sample_taken.iter_mut() {
            if *s == scenario {
                saw_bucket = true;
                if tally.taken < self.cfg.sample_cap && tally.chased < CHASE_CALLS {
                    tally.chased += 1;
                    return true;
                }
            }
        }
        !saw_bucket // nothing recorded for this scenario yet → record
    }

    /// Whether a sample is still wanted for `(scenario, class, case)`
    /// (bucket < cap) — a cheap pre-check so the caller can skip the expensive
    /// HTML render when the bucket is already full. The case sub-buckets are
    /// independent, so each distinct failure mode gets up to `sample_cap` flows.
    pub fn wants_sample(&self, scenario: ScenarioId, class: &ResultClass, case: &str) -> bool {
        let g = self.inner.lock().unwrap();
        let key = (scenario, class.label(), case.to_string());
        g.sample_taken.get(&key).map_or(0, |t| t.taken) < self.cfg.sample_cap
    }

    /// Record one completed call: bump its `(scenario, class, case)` count and
    /// its accepted-branch or failed-step count, fold its end-to-end time and
    /// checkpoints into the histograms, and store its sample if the bucket
    /// still has room. `case` is the outcome's bounded
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
    ) {
        let class = ResultClass::from(outcome);
        let label = class.label();
        let mut g = self.inner.lock().unwrap();
        *g.counts.entry((scenario, label.clone(), case.to_string())).or_default() += 1;
        match outcome {
            CallOutcome::Ok { branch } => *g.branches.entry((scenario, branch)).or_default() += 1,
            CallOutcome::Step { at, .. } => {
                *g.failed_steps.entry((scenario, *at)).or_default() += 1
            }
            _ => {}
        }
        // A rejected call never ran: it has no end-to-end time to fold in.
        if class != ResultClass::Rejected {
            g.e2e.entry(scenario).or_insert_with(Hist::call).record(e2e.as_secs_f64() * 1000.0);
        }
        for (name, d) in checkpoints {
            g.checkpoints
                .entry((scenario, name))
                .or_insert_with(Hist::call)
                .record(d.as_secs_f64() * 1000.0);
        }
        if let Some(sample) = sample {
            let key = (scenario, label, case.to_string());
            let tally = g.sample_taken.entry(key.clone()).or_default();
            if tally.taken < self.cfg.sample_cap {
                *tally = BucketTally { taken: tally.taken + 1, chased: 0 };
                g.samples.entry(key).or_default().push(sample);
            }
        }
    }

    /// Fold the measured SIP round trips of one finished `scenario` call into
    /// their `(scenario, exchange)` histograms.
    pub fn record_rtts(&self, scenario: ScenarioId, rtts: &[(Exchange, Duration)]) {
        if rtts.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        for (exchange, rtt) in rtts {
            g.rtt
                .entry((scenario, *exchange))
                .or_insert_with(Hist::round_trip)
                .record(rtt.as_secs_f64() * 1000.0);
        }
    }

    /// Hold the reporter's lock until the returned guard drops (a test
    /// stands in for a report snapshot).
    #[cfg(test)]
    pub(crate) fn hold_lock_for_test(&self) -> impl Sized + '_ {
        self.inner.lock().unwrap()
    }

    // -- Prometheus ---------------------------------------------------------

    /// The scenarios of the run's mix: each one's series are written from the
    /// first scrape, at 0 before its first call.
    pub fn declare_scenarios(&self, scenarios: impl IntoIterator<Item = ScenarioId>) {
        let mut g = self.inner.lock().unwrap();
        g.declared.extend(scenarios);
    }

    /// Name the load stream every series carries (`main` until set).
    pub fn set_stream(&self, stream: &str) {
        self.inner.lock().unwrap().stream = Some(stream.to_string());
    }

    // -- Final on-disk report ----------------------------------------------

    /// Write the on-disk report under `out_dir`: per-`(scenario, class)`
    /// callflow HTML pages, an `index.html` (counts table, latency percentiles,
    /// links), and a `summary.md`. The report is rendered under the reporter's
    /// lock and written after it is released, so recording calls never wait on
    /// the disk.
    pub fn finalize(&self, out_dir: &Path) -> std::io::Result<()> {
        let ReportFiles { pages, index, summary } = self.render_report();
        std::fs::create_dir_all(out_dir)?;
        for (rel, html) in pages {
            let page = out_dir.join(&rel);
            if let Some(parent) = page.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&page, html.as_bytes())?;
        }
        std::fs::write(out_dir.join("index.html"), index)?;
        std::fs::write(out_dir.join("summary.md"), summary)
    }

    /// The on-disk report as text: every sample page (by run-dir-relative
    /// path), `index.html` and `summary.md`.
    fn render_report(&self) -> ReportFiles {
        use std::fmt::Write as _;
        let g = self.inner.lock().unwrap();

        // Each sample's callflow HTML, split by case (which failure mode).
        let mut pages: Vec<(String, Arc<str>)> = Vec::new();
        let mut links: BTreeMap<Bucket, Vec<String>> = BTreeMap::new();
        for ((scenario, class, case), samples) in &g.samples {
            for (i, s) in samples.iter().enumerate() {
                let rel = sample_rel(scenario, class, case, i);
                let html = match &s.html {
                    Some(html) => html.clone(),
                    // No rendered flow (sample taken on a non-recording call) —
                    // a stub so the detail is still linked.
                    None => Arc::from(format!(
                        "<html><body><h3>{scenario} / {class} / {case} #{i}</h3><p>{}</p>\
                         <p>e2e: {:.1} ms</p></body></html>",
                        s.detail.as_deref().unwrap_or("(no detail)"),
                        s.e2e_ms
                    )),
                };
                pages.push((rel.clone(), html));
                links.entry((scenario, class.clone(), case.clone())).or_default().push(rel);
            }
        }

        // index.html
        let mut idx = String::new();
        idx.push_str(
            "<html><head><meta charset=\"utf-8\"><title>loadgen report</title>\
            <style>body{font-family:sans-serif}table{border-collapse:collapse}\
            td,th{border:1px solid #ccc;padding:4px 8px}.ok{color:#070}.nok{color:#a00}</style>\
            </head><body><h1>loadgen report</h1>",
        );
        idx.push_str(
            "<h2>Results by scenario × class × case</h2>\
            <p>The <b>case</b> column splits a class into its distinct failure modes (RFC rule id, \
            failed check, failed step) — each keeps its own first-N samples.</p>\
            <table>\
            <tr><th>scenario</th><th>class</th><th>case</th><th>count</th><th>samples</th></tr>",
        );
        for ((scenario, class, case), n) in &g.counts {
            let cls = if ResultClass::label_is_ok(class) { "ok" } else { "nok" };
            let sample_links = links
                .get(&(scenario, class.clone(), case.clone()))
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
                 <td>{n}</td><td>{sample_links}</td></tr>"
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

        // summary.md (writing into a String cannot fail).
        let mut md = String::new();
        let _ = writeln!(md, "# loadgen summary\n");
        let _ = writeln!(md, "## Results by scenario × class × case\n");
        let _ = writeln!(md, "(case = the class's distinct failure mode)\n");
        let _ = writeln!(md, "| scenario | class | case | count |");
        let _ = writeln!(md, "|---|---|---|---|");
        for ((scenario, class, case), n) in &g.counts {
            let _ = writeln!(md, "| {scenario} | {class} | {case} | {n} |");
        }
        let _ = writeln!(md, "\n## Latency (ms)\n");
        let _ = writeln!(md, "| scenario | n | mean | p50 | p90 | p99 | max |");
        let _ = writeln!(md, "|---|---|---|---|---|---|---|");
        for (scenario, h) in &g.e2e {
            let _ = writeln!(
                md,
                "| {scenario} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                h.total,
                h.mean_ms(),
                h.quantile_ms(0.5),
                h.quantile_ms(0.9),
                h.quantile_ms(0.99),
                h.max
            );
        }
        ReportFiles { pages, index: idx, summary: md }
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
            .map(|((scenario, class, case), n)| CountRow {
                scenario: scenario.to_string(),
                class: class.clone(),
                case: case.clone(),
                count: *n,
                ok: ResultClass::label_is_ok(class),
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

        // The stored-sample links, keyed by their `(scenario, class, case)`
        // bucket — the SAME layout `finalize` writes to disk
        // (`sample_rel` is the single path authority).
        let samples = g
            .samples
            .iter()
            .map(|((scenario, class, case), stored)| {
                let pages =
                    (0..stored.len()).map(|i| sample_rel(scenario, class, case, i)).collect();
                SampleGroup {
                    scenario: scenario.to_string(),
                    class: class.clone(),
                    case: case.clone(),
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
    /// strands `finished: false` on disk.
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

    /// Total count for a `(scenario, class)` bucket, summed across all case
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

    /// Number of stored samples for a `(scenario, class)` bucket, across all
    /// case sub-buckets.
    pub fn sample_count(&self, scenario: ScenarioId, class: &ResultClass) -> u32 {
        let label = class.label();
        self.inner
            .lock()
            .unwrap()
            .sample_taken
            .iter()
            .filter(|((s, c, _), _)| *s == scenario && *c == label)
            .map(|(_, t)| t.taken)
            .sum()
    }

    /// Total completed calls across all buckets.
    pub fn total_calls(&self) -> u64 {
        self.inner.lock().unwrap().counts.values().sum()
    }
}

mod prometheus;

#[cfg(test)]
mod tests;
