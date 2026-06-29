//! Chaos-event correlation.
//!
//! A bounded log of "a fault was injected at instant T" markers, pushed in by the
//! chaos driver (endurance.sh / chaos.sh) via the loadgen's HTTP `POST /chaos`
//! endpoint. A finished call is then auto-classified [`ChaosTag::Near`] (its
//! lifetime overlapped an injected fault within a tolerance — likely acceptable
//! kill collateral) vs [`ChaosTag::Clear`] (a genuine SUT signal).
//!
//! Why a pushed marker, not a timestamp reconciliation: the sampled callflow's
//! Call-ID/tag ms-counter sits on an unknown base (~41 days off Unix), so "this
//! call connected near the kill" could not be proven against the kill wall-clock.
//! Flagging the loadgen at the kill instant sidesteps it entirely — the loadgen
//! timestamps BOTH the markers and the calls on its own monotonic clock, so the
//! overlap is exact. The near bucket is still counted (never discarded), just
//! split apart so the analysis can focus on the `clear` failures without hand-
//! triaging every kill-collateral call.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The near/clear dimension a finished call carries alongside its
/// `(scenario, result-class)` bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChaosTag {
    /// No injected fault overlapped this call's lifetime (± tolerance) — treated
    /// as a genuine signal by the analysis.
    Clear,
    /// A fault was injected within `tolerance` of this call's lifetime — likely
    /// acceptable kill collateral, auto-bucketed apart so it needn't be triaged
    /// by hand (still counted, so it can be revisited later).
    Near,
}

impl ChaosTag {
    /// The stable label string (Prometheus `chaos` value + sample dir name).
    pub fn label(self) -> &'static str {
        match self {
            ChaosTag::Clear => "clear",
            ChaosTag::Near => "near",
        }
    }
}

struct ChaosEvent {
    at: Instant,
    kind: String,
    /// The faulted target (e.g. `b2bua-worker-1`). Recorded now for the planned
    /// v2 refinement — narrowing `Near` to calls whose Record-Route `w_pri`
    /// cookie names the killed worker — but the v1 classifier is time-window only.
    #[allow(dead_code)]
    target: Option<String>,
}

/// A bounded ring of recent chaos markers + a near/clear classifier.
pub struct ChaosLog {
    events: Mutex<VecDeque<ChaosEvent>>,
    cap: usize,
    tolerance: Duration,
    phase_tolerance: Duration,
    total: AtomicU64,
}

impl ChaosLog {
    /// A log that classifies a call `Near` when an injected fault falls within
    /// `tolerance` of the call's lifetime. Retains the last 256 markers (kills
    /// are infrequent — every chaos interval — so this spans many cycles).
    /// `phase_tolerance` defaults to 200 ms (the finer window the per-phase
    /// classifier uses); set it with [`with_phase_tolerance`](Self::with_phase_tolerance).
    pub fn new(tolerance: Duration) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            cap: 256,
            tolerance,
            phase_tolerance: Duration::from_millis(200),
            total: AtomicU64::new(0),
        }
    }

    /// Set the per-phase tolerance: how close a *dialog-state transition* must be
    /// to a fault for the call to count as `Near` on the transition rule. This is
    /// the "the state change didn't have time to propagate" window (a transition
    /// killed within it is normal distributed-systems behavior, not a SUT defect),
    /// and is deliberately tighter than the coarse call-lifetime [`tolerance`](Self::tolerance).
    pub fn with_phase_tolerance(mut self, phase_tolerance: Duration) -> Self {
        self.phase_tolerance = phase_tolerance;
        self
    }

    pub fn tolerance(&self) -> Duration {
        self.tolerance
    }

    pub fn phase_tolerance(&self) -> Duration {
        self.phase_tolerance
    }

    /// Record a fault marker at `Instant::now()` (the moment the chaos driver
    /// flagged it). Bounded: the oldest marker drops once past `cap`.
    pub fn record(&self, kind: impl Into<String>, target: Option<String>) {
        let mut g = self.events.lock().unwrap();
        g.push_back(ChaosEvent { at: Instant::now(), kind: kind.into(), target });
        while g.len() > self.cap {
            g.pop_front();
        }
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Coarse classifier: `Near` iff any marker fell within `tolerance` of the
    /// call's lifetime `[start, end]`. Used as the fallback / by the simple tests;
    /// the driver prefers [`classify_call`](Self::classify_call).
    pub fn classify(&self, start: Instant, end: Instant) -> ChaosTag {
        let lo = start.checked_sub(self.tolerance).unwrap_or(start);
        let hi = end + self.tolerance;
        let g = self.events.lock().unwrap();
        if g.iter().any(|e| e.at >= lo && e.at <= hi) {
            ChaosTag::Near
        } else {
            ChaosTag::Clear
        }
    }

    /// Per-phase classifier — the precise "was this failure explained by a fault
    /// landing on a fragile moment?" test. A call is `Near` iff some retained
    /// marker satisfies EITHER:
    ///
    /// 1. **Transition coincidence** — it fell within `phase_tolerance` of a
    ///    recorded dialog-state transition (`connected`, `reinvited`,
    ///    `transferred`, …). A transition killed that close had no time to
    ///    propagate/replicate, and SIP retransmission normally recovers it — so
    ///    it is acceptable kill collateral, NOT a SUT defect.
    /// 2. **Interrupted setup** — it fell inside the call's lifetime while the
    ///    call had not yet reached `connected`. An INVITE killed mid-setup
    ///    (the pre-connect 408 family) is likewise acceptable.
    ///
    /// A call that was *stably connected* across the fault (no transition near it,
    /// already past `connected`) stays `Clear` — if such a call fails, the kill
    /// did not catch it mid-transition, so it is a genuine signal to investigate.
    pub fn classify_call(
        &self,
        start: Instant,
        end: Instant,
        phases: &[(&'static str, Instant)],
    ) -> ChaosTag {
        let g = self.events.lock().unwrap();
        for e in g.iter() {
            // (1) transition coincidence (± phase_tolerance)
            let near_transition = phases.iter().any(|(_, at)| {
                let lo = at.checked_sub(self.phase_tolerance).unwrap_or(*at);
                e.at >= lo && e.at <= *at + self.phase_tolerance
            });
            if near_transition {
                return ChaosTag::Near;
            }
            // (2) interrupted setup: marker within the lifetime, before `connected`
            if e.at >= start && e.at <= end {
                let connected_before =
                    phases.iter().any(|(n, at)| *n == "connected" && *at <= e.at);
                if !connected_before {
                    return ChaosTag::Near;
                }
            }
        }
        ChaosTag::Clear
    }

    /// Total markers recorded over the run (monotonic; not bounded by `cap`).
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Prometheus surface: total markers recorded by kind, so the dashboard can
    /// confirm the loadgen actually received the chaos flags.
    pub fn render_prometheus(&self) -> String {
        let g = self.events.lock().unwrap();
        let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
        for e in g.iter() {
            *by_kind.entry(e.kind.as_str()).or_default() += 1;
        }
        let mut out = String::new();
        out.push_str("# HELP loadgen_chaos_markers_total Chaos markers recorded by the loadgen.\n");
        out.push_str("# TYPE loadgen_chaos_markers_total counter\n");
        out.push_str(&format!("loadgen_chaos_markers_total {}\n", self.total()));
        out.push_str("# HELP loadgen_chaos_markers_retained Chaos markers currently retained, by kind.\n");
        out.push_str("# TYPE loadgen_chaos_markers_retained gauge\n");
        for (k, n) in &by_kind {
            out.push_str(&format!("loadgen_chaos_markers_retained{{kind=\"{k}\"}} {n}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_within_tolerance_is_near_outside_is_clear() {
        let log = ChaosLog::new(Duration::from_millis(500));
        let base = Instant::now();
        // A marker at `base`.
        log.record("kill_worker", Some("b2bua-worker-1".into()));

        // A call whose window straddles the marker → Near.
        assert_eq!(log.classify(base - Duration::from_secs(1), base + Duration::from_secs(1)), ChaosTag::Near);
        // A call that ended 300ms before the marker (< 500ms tol) → Near.
        assert_eq!(
            log.classify(base - Duration::from_secs(2), base - Duration::from_millis(300)),
            ChaosTag::Near
        );
        // A call that ended 800ms before the marker (> 500ms tol) → Clear.
        assert_eq!(
            log.classify(base - Duration::from_secs(2), base - Duration::from_millis(800)),
            ChaosTag::Clear
        );
        // A call that started 800ms after the marker (the post-reboot fresh call
        // the CSeq-desync bug rides) → Clear: it must NOT be excused.
        assert_eq!(
            log.classify(base + Duration::from_millis(800), base + Duration::from_secs(2)),
            ChaosTag::Clear
        );
    }

    #[test]
    fn per_phase_classifier_excuses_transitions_and_setup_but_not_stable_calls() {
        let log = ChaosLog::new(Duration::from_secs(5)).with_phase_tolerance(Duration::from_millis(200));
        let kill = Instant::now();
        log.record("kill_worker", None);

        // (1) a call whose `reinvited` transition was 150ms before the kill → Near
        //     (the transition had no time to propagate — acceptable collateral).
        let phases_txn = [("connected", kill - Duration::from_secs(3)), ("reinvited", kill - Duration::from_millis(150))];
        assert_eq!(
            log.classify_call(kill - Duration::from_secs(4), kill + Duration::from_secs(1), &phases_txn),
            ChaosTag::Near
        );

        // (2) a call still in SETUP at the kill (started, never reached connected) → Near.
        let phases_setup: [(&'static str, Instant); 0] = [];
        assert_eq!(
            log.classify_call(kill - Duration::from_secs(2), kill + Duration::from_secs(1), &phases_setup),
            ChaosTag::Near
        );

        // (3) a STABLY-connected call across the kill — connected 10s before, no
        //     transition near the kill → Clear (a genuine signal, must be triaged).
        let phases_stable = [("connected", kill - Duration::from_secs(10))];
        assert_eq!(
            log.classify_call(kill - Duration::from_secs(20), kill + Duration::from_secs(20), &phases_stable),
            ChaosTag::Clear
        );

        // (4) a fresh post-kill call: connected 2s AFTER the kill, far from it →
        //     Clear (the post-reboot CSeq-desync calls — must stay visible).
        let phases_post = [("connected", kill + Duration::from_secs(2)), ("reinvited", kill + Duration::from_secs(3))];
        assert_eq!(
            log.classify_call(kill + Duration::from_millis(900), kill + Duration::from_secs(5), &phases_post),
            ChaosTag::Clear
        );
    }

    #[test]
    fn empty_log_is_always_clear() {
        let log = ChaosLog::new(Duration::from_millis(500));
        let now = Instant::now();
        assert_eq!(log.classify(now, now), ChaosTag::Clear);
        assert_eq!(log.total(), 0);
    }

    #[test]
    fn ring_is_bounded_but_total_is_monotonic() {
        let log = ChaosLog::new(Duration::from_millis(10));
        for _ in 0..300 {
            log.record("kill_worker", None);
        }
        assert_eq!(log.total(), 300);
        assert!(log.events.lock().unwrap().len() <= log.cap);
    }
}
