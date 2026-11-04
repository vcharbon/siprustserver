//! Bounded-memory reporting: per-`(scenario, class)` counters, per-scenario
//! accepted-branch and failed-step counters, end-to-end and
//! named-checkpoint latency histograms, per-`(scenario, exchange)` SIP
//! round-trip histograms, a bounded sample store (the first N
//! callflows per `(scenario, class)` — **including OK**, so OK vs failing flows
//! are comparable), the run's time series and last failures, a live
//! Prometheus `/metrics` surface, and the load run index the report pages are
//! rendered from (served live, written to disk).
//!
//! The memory ceiling is structural: counters and histograms are fixed-size;
//! samples are capped per bucket; and the sampling gate
//! ([`Reporter::should_record`]) records a fixed share of the calls from the
//! first call on and the first `sample_cap` calls of each scenario, whatever the
//! call hold time and the outcomes. Recording is per-call (freed when the call's
//! binder drops): nothing accumulates.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::class::{CallOutcome, ResultClass};
use crate::hist::Hist;
use crate::mux::Exchange;
use crate::scenarios::ScenarioId;
use scenario_harness::actor::FailedAt;

// ---------------------------------------------------------------------------
// Reporter
// ---------------------------------------------------------------------------

/// The sample bounds of a [`Reporter`].
#[derive(Clone)]
pub struct ReporterCfg {
    /// Max stored callflow samples per `(scenario, class)` bucket.
    pub sample_cap: u32,
    /// Record 1 call of each scenario in `background_record_every`, from its
    /// first call to its last, so a late-appearing error class still gets a
    /// sample. 0 disables.
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
    /// Samples stored per bucket.
    sample_taken: BTreeMap<Bucket, u32>,
    /// Calls placed per scenario, counted by the sampling gate.
    placed: BTreeMap<ScenarioId, u64>,
    /// Calls that ran recorded, per scenario.
    recorded: BTreeMap<ScenarioId, u64>,
    /// Calls that ended on an accepted branch before the RFC audit, per
    /// scenario: the calls the audit judges.
    accepted: BTreeMap<ScenarioId, u64>,
    samples: BTreeMap<Bucket, Vec<RenderedSample>>,
    /// Per-scenario Test-case check-verdict tally over the SAMPLED calls:
    /// `(passed_all, failed_any)`. Fed only for calls whose attached case had
    /// checks that were actually evaluated (sampled OK/check-fail calls).
    check_verdicts: BTreeMap<ScenarioId, (u64, u64)>,
    /// Accepted calls whose recording the RFC audit judged, per scenario.
    audited: BTreeMap<ScenarioId, u64>,
    /// Of those, the calls the audit failed, per bucket.
    audited_failed: BTreeMap<Bucket, u64>,
    /// Per-scenario structural-waiver campaign tally (ADR-0024 §6): a waiver's
    /// "was ever used" OR-aggregated across the scenario's sampled calls, so the
    /// unused-waiver gate is per CAMPAIGN, never per call.
    waivers: BTreeMap<ScenarioId, crate::waiver::CampaignWaivers>,
    /// The time series, the run-wide latency it windows and the last failures.
    live: live::Live,
    /// Per scenario, the message ladder of its calls.
    ladders: BTreeMap<ScenarioId, ladder::LadderAgg>,
}

/// The bounded-memory load-test reporter. Cloneable handle via `Arc`.
pub struct Reporter {
    cfg: ReporterCfg,
    /// The run's start: every time the reporter states is counted from it.
    start: Instant,
    inner: Mutex<Inner>,
    inflight: AtomicU64,
    started: AtomicU64,
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
            start: Instant::now(),
            inner: Mutex::new(Inner::default()),
            inflight: AtomicU64::new(0),
            started: AtomicU64::new(0),
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
    /// trace? True for 1 call of its scenario in `background_record_every` (0:
    /// none) and for the first `sample_cap` calls of its scenario. The calls
    /// that record depend only on the scenario's call count, never on the
    /// outcomes: the loadgen's recording cost stays flat through an error burst.
    pub fn should_record(&self, scenario: ScenarioId) -> bool {
        let every = self.cfg.background_record_every;
        let mut g = self.inner.lock().unwrap();
        let nth = *g.placed.entry(scenario).and_modify(|n| *n += 1).or_insert(1) - 1;
        (every != 0 && nth.is_multiple_of(every)) || nth < u64::from(self.cfg.sample_cap)
    }

    /// Whether a sample is still wanted for `(scenario, class, case)`
    /// (bucket < cap) — a cheap pre-check so the caller can skip the expensive
    /// HTML render when the bucket is already full. The case sub-buckets are
    /// independent, so each distinct failure mode gets up to `sample_cap` flows.
    pub fn wants_sample(&self, scenario: ScenarioId, class: &ResultClass, case: &str) -> bool {
        let g = self.inner.lock().unwrap();
        let key = (scenario, class.label(), case.to_string());
        g.sample_taken.get(&key).copied().unwrap_or(0) < self.cfg.sample_cap
    }

    /// Record one completed call: bump its `(scenario, class, case)` count and
    /// its accepted-branch or failed-step count, fold its end-to-end time and
    /// checkpoints into the histograms, and store its sample if the bucket
    /// still has room, and fold its `evidence` into the recorded and audited
    /// tallies. `case` is the outcome's bounded
    /// discriminator ([`CallOutcome::case`] — the driver computes it once, with
    /// the call's phase trail).
    pub fn record(
        &self,
        scenario: ScenarioId,
        outcome: &CallOutcome,
        case: &str,
        e2e: Duration,
        checkpoints: &[(&'static str, Duration)],
        evidence: CallEvidence,
    ) {
        let CallEvidence { recorded, accepted, sample } = evidence;
        let class = ResultClass::from(outcome);
        let label = class.label();
        let mut g = self.inner.lock().unwrap();
        *g.counts.entry((scenario, label.clone(), case.to_string())).or_default() += 1;
        if recorded {
            *g.recorded.entry(scenario).or_default() += 1;
        }
        if accepted {
            *g.accepted.entry(scenario).or_default() += 1;
        }
        if accepted && recorded {
            *g.audited.entry(scenario).or_default() += 1;
            if class == ResultClass::RfcAuditFail {
                let bucket = (scenario, label.clone(), case.to_string());
                *g.audited_failed.entry(bucket).or_default() += 1;
            }
        }
        match outcome {
            CallOutcome::Ok { branch } => *g.branches.entry((scenario, branch)).or_default() += 1,
            CallOutcome::Step { at, .. } => {
                *g.failed_steps.entry((scenario, *at)).or_default() += 1
            }
            _ => {}
        }
        // A rejected call never ran: it has no end-to-end time to fold in.
        if class != ResultClass::Rejected {
            let ms = e2e.as_secs_f64() * 1000.0;
            g.e2e.entry(scenario).or_insert_with(Hist::call).record(ms);
            g.live.latency.record(ms);
        }
        for (name, d) in checkpoints {
            g.checkpoints
                .entry((scenario, name))
                .or_insert_with(Hist::call)
                .record(d.as_secs_f64() * 1000.0);
        }
        let key = (scenario, label, case.to_string());
        let mut page = None;
        if let Some(sample) = sample {
            let taken = g.sample_taken.entry(key.clone()).or_default();
            if *taken < self.cfg.sample_cap {
                *taken += 1;
                let stored = g.samples.entry(key.clone()).or_default();
                stored.push(sample);
                page = Some(sample_rel(scenario, &key.1, case, stored.len() - 1));
            }
        }
        // A refused key is no SUT failure: it never pushes one out of the list.
        if !class.is_ok() && class != ResultClass::Rejected {
            let at_ms = self.ms_since_start(Instant::now());
            g.live.note_failure(at_ms, &key, page);
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

    /// Fold one finished call's messages into its scenario's ladder.
    pub fn record_messages(
        &self,
        scenario: ScenarioId,
        messages: crate::mux::CallMessages,
        outcome: &CallOutcome,
    ) {
        let mut g = self.inner.lock().unwrap();
        g.ladders.entry(scenario).or_default().fold(messages, outcome);
    }

    /// A retransmission of the request on `arrow` of a `scenario` call,
    /// answered again by the mux after the call ended.
    pub fn record_late_answer(&self, scenario: ScenarioId, arrow: &crate::mux::ArrowKey) {
        let mut g = self.inner.lock().unwrap();
        g.ladders.entry(scenario).or_default().late(arrow);
    }

    /// Where a `scenario` call's late re-answers go: its scenario's ladder.
    pub fn late_sink(self: &Arc<Self>, scenario: ScenarioId) -> crate::mux::LateSink {
        let reporter = self.clone();
        Arc::new(move |arrow| reporter.record_late_answer(scenario, arrow))
    }

    /// The run configured the retransmit engine for `scenario`, or not.
    pub fn declare_retransmit(&self, scenario: ScenarioId, engine: bool) {
        let mut g = self.inner.lock().unwrap();
        g.ladders.entry(scenario).or_insert_with(|| ladder::LadderAgg::new(engine)).engine = engine;
    }

    /// Name the load stream every series carries (`main` until set).
    pub fn set_stream(&self, stream: &str) {
        self.inner.lock().unwrap().stream = Some(stream.to_string());
    }
}

mod disk;
mod evidence;
mod index;
mod inspect;
mod ladder;
mod live;
mod pages;
mod prometheus;
mod series;
pub use evidence::{CallEvidence, RenderedSample};
pub use series::SERIES_TICK;

#[cfg(test)]
mod tests;
