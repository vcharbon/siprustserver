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
//! timestamps BOTH the markers and the calls on **one** monotonic-anchored
//! [`Clock`] (the process-wide `Clock::system()` shared with the recorders), so
//! the overlap is exact AND a marker renders on the very axis the frames do —
//! even if the host wall clock STEPS mid-run (the WSL2 endurance hazard), the
//! marker and the frames drift together. The near bucket is still counted (never
//! discarded), just split apart so the analysis can focus on the `clear` failures
//! without hand-triaging every kill-collateral call.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sip_clock::Clock;

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
    /// Epoch ms of the fault **on the loadgen [`Clock`] axis** — the kill instant
    /// for a back-dated marker ([`record_at`](ChaosLog::record_at)), else the
    /// receipt instant. Used to render the marker on a callflow's timeline: the
    /// recorders stamp frames off the SAME shared `Clock`, so frame and marker
    /// share one axis and stay aligned even across a host wall-clock step. The
    /// `at` Instant remains the classifier's monotonic key.
    wall_ms: i64,
    kind: String,
    /// The faulted target (e.g. `b2bua-worker-1`). Recorded now for the planned
    /// v2 refinement — narrowing `Near` to calls whose Record-Route `w_pri`
    /// cookie names the killed worker — but the v1 classifier is time-window only.
    target: Option<String>,
}

/// A bounded ring of recent chaos markers + a near/clear classifier.
pub struct ChaosLog {
    events: Mutex<VecDeque<ChaosEvent>>,
    cap: usize,
    phase_tolerance: Duration,
    /// The process-wide monotonic-anchored clock shared with the call recorders,
    /// so a marker's rendered `wall_ms` lands on the SAME axis as the frames.
    clock: Clock,
    total: AtomicU64,
}

impl ChaosLog {
    /// A log that classifies a call `Near` when an injected fault falls on a
    /// fragile moment of its lifetime (see [`classify_call`](Self::classify_call)).
    /// Retains the last 256 markers (kills are infrequent — every chaos interval —
    /// so this spans many cycles). `phase_tolerance` defaults to 200 ms; set it
    /// with [`with_phase_tolerance`](Self::with_phase_tolerance).
    ///
    /// `clock` must be the same `Clock` the call recorders use, so a marker's
    /// rendered `wall_ms` shares the frames' timeline axis.
    pub fn new(clock: Clock) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            cap: 256,
            phase_tolerance: Duration::from_millis(200),
            clock,
            total: AtomicU64::new(0),
        }
    }

    /// Set the per-phase tolerance: how close a *dialog-state transition* must be
    /// to a fault for the call to count as `Near` on the transition rule. This is
    /// the "the state change didn't have time to propagate" window (a transition
    /// killed within it is normal distributed-systems behavior, not a SUT defect).
    pub fn with_phase_tolerance(mut self, phase_tolerance: Duration) -> Self {
        self.phase_tolerance = phase_tolerance;
        self
    }

    pub fn phase_tolerance(&self) -> Duration {
        self.phase_tolerance
    }

    /// Record a fault marker at now (the moment the chaos driver flagged it),
    /// stamped off the shared [`Clock`] so it shares the frames' axis. Bounded:
    /// the oldest marker drops once past `cap`.
    pub fn record(&self, kind: impl Into<String>, target: Option<String>) {
        self.push(Instant::now(), self.clock.now_ms(), kind.into(), target);
    }

    /// Record a fault marker **back-dated to the kill instant** `kill_unix_ms`
    /// (Unix epoch milliseconds, captured by the chaos script's `date` at the
    /// `kubectl delete pod`). The marker landed over a port-forward, so the POST
    /// arrives some delay after the kill; back-dating makes the marker robust to
    /// ANY plumbing latency (PF retries, extra hops) rather than recording receipt.
    ///
    /// `kill_unix_ms` is an EXTERNAL wall-clock (Unix) timestamp, so measuring how
    /// long ago it was needs a real wall read — this is the one documented place
    /// loadgen reads [`SystemTime`] directly (per `sip_clock`: read `SystemTime`
    /// only to reconcile an external wall clock). We then map that delay back onto
    /// the shared `Clock` axis so the STORED `wall_ms` still aligns with the
    /// frames even if the host clock has stepped since the loadgen anchored:
    /// `delay = max(0, wall_now − kill)`, `at = Instant::now() − delay`,
    /// `wall_ms = clock.now_ms() − delay`. `saturating_sub` keeps a future-dated
    /// kill (clock skew) at delay 0 → marker at ~now, no panic.
    pub fn record_at(&self, kind: impl Into<String>, target: Option<String>, kill_unix_ms: u64) {
        let wall_now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let delay_ms = wall_now_ms.saturating_sub(kill_unix_ms);
        let at = Instant::now()
            .checked_sub(Duration::from_millis(delay_ms))
            .unwrap_or_else(Instant::now);
        let wall_ms = self.clock.now_ms() - delay_ms as i64;
        self.push(at, wall_ms, kind.into(), target);
    }

    fn push(&self, at: Instant, wall_ms: i64, kind: String, target: Option<String>) {
        let mut g = self.events.lock().unwrap();
        g.push_back(ChaosEvent { at, wall_ms, kind, target });
        while g.len() > self.cap {
            g.pop_front();
        }
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Recent fault markers as `(wall_clock_epoch_ms, label)` for callflow
    /// rendering — the renderer drops each onto a sampled flow's wall-clock
    /// timeline (filtered to that call's window) as a Lifecycle band, so a NOK
    /// page shows exactly when the kill landed relative to the call's frames.
    /// Label is `chaos <kind>(<target>)`, e.g. `chaos kill_worker(b2bua-worker-0)`.
    pub fn markers(&self) -> Vec<(i64, String)> {
        let g = self.events.lock().unwrap();
        g.iter()
            .map(|e| {
                let label = match &e.target {
                    Some(t) => format!("chaos {}({t})", e.kind),
                    None => format!("chaos {}", e.kind),
                };
                (e.wall_ms, label)
            })
            .collect()
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
    use sip_clock::Clock;

    // A Clock for the tests: real-anchored (a live-network log). The classifier
    // tests key off the monotonic `at` Instant, so the Clock value is irrelevant
    // there; only the `markers()` wall-clock tests read it back. `#[tokio::test]`
    // (not paused) so `Clock::system()` has a runtime for its `tokio` Instant.
    fn test_log() -> ChaosLog {
        ChaosLog::new(Clock::system())
    }

    #[tokio::test]
    async fn per_phase_classifier_excuses_transitions_and_setup_but_not_stable_calls() {
        let log = test_log().with_phase_tolerance(Duration::from_millis(200));
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

    #[tokio::test]
    async fn record_at_back_dates_to_the_kill_so_a_transition_at_the_kill_is_near() {
        // The marker arrives ~1.5 s after the kill (the PF latency the live run
        // saw). Without back-dating, a `connected` transition AT the kill would
        // sit 1.5 s before the (late) marker — outside the 200 ms phase window —
        // and mis-bucket `Clear`. record_at must place the marker back at the kill.
        let log = test_log().with_phase_tolerance(Duration::from_millis(200));

        // Pretend "now" is 1.5 s after a kill: the kill's Unix ms is now − 1500.
        let now_wall_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let kill_unix_ms = now_wall_ms - 1500;
        let kill_instant = Instant::now() - Duration::from_millis(1500);

        log.record_at("kill_worker", Some("b2bua-worker-1".into()), kill_unix_ms);

        // A call whose `connected` transition coincided with the kill (1.5 s ago):
        // with the marker back-dated it is within the 200 ms phase window → Near.
        let phases = [("connected", kill_instant)];
        assert_eq!(
            log.classify_call(kill_instant - Duration::from_secs(1), Instant::now(), &phases),
            ChaosTag::Near
        );

        // Sanity: a plain `record` (receipt instant, no back-dating) would have
        // placed the marker ~now, 1.5 s after that transition → mis-bucketed Clear.
        let late = test_log().with_phase_tolerance(Duration::from_millis(200));
        late.record("kill_worker", None);
        assert_eq!(
            late.classify_call(kill_instant - Duration::from_secs(1), Instant::now(), &phases),
            ChaosTag::Clear
        );
    }

    #[tokio::test]
    async fn record_at_with_future_kill_ts_falls_back_to_now_not_panic() {
        // Clock skew: the supplied kill ts is in the future. saturating_sub keeps
        // delay at 0 → marker at ~now (no panic, no underflow).
        let log = test_log();
        let now_wall_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        log.record_at("kill_worker", None, now_wall_ms + 10_000);
        assert_eq!(log.total(), 1);
        // The marker landed at ~now (delay clamped to 0), so a call whose window
        // brackets now (no `connected` phase) is Near on the interrupted-setup rule.
        let now = Instant::now();
        assert_eq!(
            log.classify_call(now - Duration::from_millis(100), now + Duration::from_millis(100), &[]),
            ChaosTag::Near
        );
    }

    #[tokio::test]
    async fn markers_carry_back_dated_wall_clock_and_label() {
        let log = test_log();
        let now_wall_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let kill_unix_ms = now_wall_ms - 1500;
        log.record_at("kill_worker", Some("b2bua-worker-0".into()), kill_unix_ms);
        log.record("kill_proxy", None); // no-ts fallback → receipt (now) on the Clock axis

        let m = log.markers();
        assert_eq!(m.len(), 2);
        // The back-dated marker sits ~1.5 s in the past (near the kill), stamped on
        // the shared Clock axis (≈ the Unix kill instant when the clock hasn't
        // stepped), labelled with kind + target — what the callflow band renders.
        assert!(
            (m[0].0 - kill_unix_ms as i64).abs() < 500,
            "back-dated wall_ms ({}) is ~the kill instant ({kill_unix_ms})",
            m[0].0
        );
        assert_eq!(m[0].1, "chaos kill_worker(b2bua-worker-0)");
        assert_eq!(m[1].1, "chaos kill_proxy");
        // The no-ts marker is stamped at receipt (~now), ~1.5 s AFTER the back-dated
        // one — proving record_at actually back-dated rather than stamping now.
        assert!(m[1].0 >= kill_unix_ms as i64, "no-ts marker stamped at receipt (now)");
        assert!(m[1].0 - m[0].0 >= 1_000, "no-ts receipt is ~1.5 s after the back-dated kill");
    }

    #[tokio::test]
    async fn empty_log_is_always_clear() {
        let log = test_log();
        let now = Instant::now();
        assert_eq!(log.classify_call(now, now, &[]), ChaosTag::Clear);
        assert_eq!(log.total(), 0);
    }

    #[tokio::test]
    async fn ring_is_bounded_but_total_is_monotonic() {
        let log = test_log();
        for _ in 0..300 {
            log.record("kill_worker", None);
        }
        assert_eq!(log.total(), 300);
        assert!(log.events.lock().unwrap().len() <= log.cap);
    }
}
