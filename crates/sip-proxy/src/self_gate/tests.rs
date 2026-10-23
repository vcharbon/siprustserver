use super::*;
use load_shed::{simulated, SimulatedLoadControl};

// Where the bucket's time-based refill is exercised the test is
// `start_paused` and drives `tokio::time::advance`, since the bucket rides
// `tokio::time::Instant` (behaviour rides `tokio::time` — no separate fake
// clock to keep in sync).

#[test]
fn stub_always_admits() {
    let g = AlwaysAdmitGate;
    assert!(g.try_admit_external().is_admit());
    g.note_bypass(BypassKind::Emergency); // no-op, must not panic
}

/// Build a gate over a simulated sampler whose ELU the returned control sets.
fn gate(size: u32, rate: u32, elu_critical: f64) -> (EluCpsGate, SimulatedLoadControl) {
    let (sampler, ctl) = simulated();
    let g = EluCpsGate::new(
        Arc::new(sampler),
        ProxySelfGateConfig {
            cps_bucket_size: size,
            cps_bucket_rate: rate,
            elu_critical,
            ..Default::default()
        },
    );
    (g, ctl)
}

#[test]
fn defaults_match_the_ts_source() {
    let c = ProxySelfGateConfig::default();
    assert_eq!(c.elu_critical, 0.8);
    assert_eq!(c.cps_bucket_size, 50);
    assert_eq!(c.cps_bucket_rate, 100);
    assert_eq!(c.elu_smoothing_alpha, 0.2);
    assert_eq!(c.sampler_interval, Duration::from_millis(100));
}

/// Before any sample fires the EWMA is `0`, so the gate admits external
/// traffic up to the bucket capacity even with a high injected ELU (cold
/// start).
#[tokio::test(start_paused = true)]
async fn cold_start_admits_until_the_first_sample() {
    let (g, ctl) = gate(2, 0, 0.8);
    ctl.set_elu(1.0); // would shed, but no sample has fed the EWMA yet
    assert!(g.try_admit_external().is_admit());
    assert_eq!(g.elu_ewma(), 0.0, "EWMA stays 0 until the first sample()");
}

/// ELU above `elu_critical` (once sampled) sheds with `proxy_overload_intake_age` +
/// `Retry-After: 1`, and crucially does NOT spend a CPS token (step 1 is
/// before the bucket).
#[tokio::test(start_paused = true)]
async fn elu_above_critical_503s_without_spending_a_token() {
    let (g, ctl) = gate(1, 0, 0.8);
    ctl.set_elu(0.9);
    g.sample(); // seat the EWMA at 0.9 (> 0.8)
    assert_eq!(
        g.try_admit_external(),
        AdmitDecision::Reject { reason: ShedReason::IntakeAge, retry_after_sec: 1 }
    );
    assert_eq!(g.metrics().rejected_intake_age_total, 1);
    // The lone token was NOT consumed: once ELU recovers, the call admits.
    ctl.set_elu(0.0);
    g.sample();
    g.sample(); // pull the EWMA back below 0.8
    assert!(g.try_admit_external().is_admit(), "the ELU reject must not have spent the token");
}

/// At exactly `elu_critical` the gate still admits (`>` is strict).
#[tokio::test(start_paused = true)]
async fn elu_exactly_at_critical_still_admits() {
    let (g, ctl) = gate(10, 0, 0.8);
    ctl.set_elu(0.8);
    g.sample(); // EWMA seated exactly at the threshold
    assert!(g.try_admit_external().is_admit(), "elu == critical is admit (strict >)");
}

/// With ELU calm, the gate admits until the CPS bucket drains, then sheds
/// with `proxy_overload_cps`.
#[tokio::test(start_paused = true)]
async fn admits_until_the_cps_bucket_drains_then_503s_cps() {
    // Capacity 2, refill 0/s so the bucket can't top up between consumes.
    let (g, _ctl) = gate(2, 0, 0.8);
    assert!(g.try_admit_external().is_admit());
    assert!(g.try_admit_external().is_admit());
    let d = g.try_admit_external();
    // rate 0 + empty → the 60 s Retry-After fallback.
    assert_eq!(d, AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: 60 });
    assert_eq!(g.metrics().rejected_cps_total, 1);
    assert_eq!(g.metrics().external_admitted_total, 2);
}

/// The bucket refills over wall time at `rate_per_sec` (lazy refill on the
/// next consume), driven by `tokio::time::advance` — no real sleep.
#[tokio::test(start_paused = true)]
async fn the_cps_bucket_refills_over_time() {
    let (g, _ctl) = gate(1, 1, 0.8);
    assert!(g.try_admit_external().is_admit()); // drains the lone token
    assert!(!g.try_admit_external().is_admit()); // empty immediately after
    tokio::time::advance(Duration::from_secs(1)).await; // +1 token at 1/s
    assert!(g.try_admit_external().is_admit(), "a refilled token should admit");
}

/// `note_bypass` advances the per-kind counters (emergency / internal); the
/// gate state (bucket, EWMA) is untouched by a bypass.
#[test]
fn note_bypass_counts_per_kind() {
    let (g, _ctl) = gate(50, 100, 0.8);
    g.note_bypass(BypassKind::Emergency);
    g.note_bypass(BypassKind::Emergency);
    g.note_bypass(BypassKind::Internal);
    let m = g.metrics();
    assert_eq!(m.emergency_bypassed_total, 2);
    assert_eq!(m.internal_bypassed_total, 1);
    // A bypass spends no token and admits nothing.
    assert_eq!(m.external_admitted_total, 0);
    assert_eq!(m.cps_bucket_level, 50.0);
}

/// The metrics snapshot mirrors config + live counters: capacity is the
/// configured constant, the level floors at 0, and admits/rejects tally.
#[tokio::test(start_paused = true)]
async fn metrics_snapshot_reflects_config_and_counters() {
    let (g, _ctl) = gate(3, 0, 0.8);
    assert_eq!(g.metrics().cps_bucket_max, 3.0);
    assert_eq!(g.metrics().cps_bucket_level, 3.0);
    for _ in 0..3 {
        assert!(g.try_admit_external().is_admit());
    }
    assert!(!g.try_admit_external().is_admit()); // drains then sheds
    let m = g.metrics();
    assert_eq!(m.cps_bucket_level, 0.0, "a drained bucket reads as empty");
    assert_eq!(m.external_admitted_total, 3);
    assert_eq!(m.rejected_cps_total, 1);
}

/// A metrics read leaves the bucket as it was. Ten tenths of a second credited
/// one read at a time sum to 0.9999999999999999 of a token in `f64`; one
/// second credited at once is exactly one token. A read that credited the
/// bucket would make the take at 1 s refuse.
#[tokio::test(start_paused = true)]
async fn a_metrics_read_leaves_the_bucket_unchanged() {
    let (g, _ctl) = gate(1, 1, 0.8);
    assert!(g.try_admit_external().is_admit(), "drains the lone token");
    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(100)).await;
        let level = g.metrics().cps_bucket_level;
        assert!(level <= 1.0, "the projected level stays within the capacity: {level}");
    }
    assert!(g.try_admit_external().is_admit(), "one second at 1/s holds one token");
}

/// The EWMA smooths toward a sustained reading across repeated ticks
/// (alpha 0.2): the first tick seats it, subsequent ticks pull it toward the
/// new value but never past it.
#[test]
fn ewma_smooths_across_repeated_samples() {
    let (g, ctl) = gate(50, 100, 0.8);
    ctl.set_elu(1.0);
    g.sample(); // seats at 1.0
    assert!((g.elu_ewma() - 1.0).abs() < 1e-9);
    ctl.set_elu(0.0);
    g.sample(); // 0.2*0 + 0.8*1.0 = 0.8
    assert!((g.elu_ewma() - 0.8).abs() < 1e-9);
    g.sample(); // 0.2*0 + 0.8*0.8 = 0.64
    assert!((g.elu_ewma() - 0.64).abs() < 1e-9);
}

/// The intake sampler normalizes the window's max dequeue age against
/// `AGE_CRITICAL_MS` (max wins over later smaller ages; ≥ critical clamps
/// to 1.0) and reports a structurally-`0` GC fraction.
#[test]
fn intake_sampler_normalizes_max_age_against_critical() {
    let rec = IntakeAgeRecorder::default();
    let s = IntakeAgeSampler::new(rec.clone());
    assert_eq!(s.elu(), 0.0, "no traffic reads idle");
    assert_eq!(s.gc_fraction(), 0.0);
    rec.record(250);
    rec.record(100); // smaller later age must not shrink the window max
    assert!((s.elu() - 0.5).abs() < 1e-9, "250ms / 500ms critical = 0.5");
    rec.record(AGE_CRITICAL_MS * 4); // way past critical clamps to 1.0
    assert_eq!(s.elu(), 1.0);
}

/// Each `elu()` read drains the recorder: a second read with no new traffic
/// reads `0` (the signal is per-window, not sticky).
#[test]
fn intake_sampler_read_drains_the_window() {
    let rec = IntakeAgeRecorder::default();
    let s = IntakeAgeSampler::new(rec.clone());
    rec.record(AGE_CRITICAL_MS);
    assert_eq!(s.elu(), 1.0);
    assert_eq!(s.elu(), 0.0, "the read must have swapped the max to 0");
    assert_eq!(rec.drain_max(), 0);
}

/// The production seam end-to-end: an `EluCpsGate::intake` gate over a
/// recorder the "dequeue path" feeds. Sustained critical ages sampled into
/// the EWMA flip the gate to `proxy_overload_intake_age`; once traffic drains
/// (empty windows read 0) the samples pull the EWMA back and it admits.
#[tokio::test(start_paused = true)]
async fn intake_gate_sheds_on_sustained_dequeue_age_then_recovers() {
    let rec = IntakeAgeRecorder::default();
    let g = EluCpsGate::intake(rec.clone(), ProxySelfGateConfig::default());
    assert!(g.try_admit_external().is_admit(), "calm intake admits");

    // Saturated intake: every sample window sees a ≥ critical dequeue age.
    for _ in 0..10 {
        rec.record(AGE_CRITICAL_MS);
        g.sample();
    }
    assert!(
        g.elu_ewma() > 0.8,
        "sustained critical age must cross elu_critical, got {}",
        g.elu_ewma()
    );
    let d = g.try_admit_external();
    assert!(matches!(d, AdmitDecision::Reject { reason: ShedReason::IntakeAge, .. }), "{d:?}");

    // Intake drains: empty windows sample 0 and the EWMA recovers.
    for _ in 0..10 {
        g.sample();
    }
    assert!(g.try_admit_external().is_admit(), "a drained intake must admit again");
}

/// The end-to-end injected-ELU → running-sampler-task → shed loop: (like
/// the b2bua overload signal) this injects the `simulated()` sampler and
/// spawns the real 100 ms `sample()` task, then advances the paused clock
/// to drive the injected ELU through the task into the EWMA and observe the
/// gate flip to shedding. Pins the sampler-task seam the runner wires.
#[tokio::test(start_paused = true)]
async fn running_sampler_task_drives_the_gate_to_shed_on_injected_elu() {
    let (sampler, ctl) = simulated();
    let g = EluCpsGate::new(
        Arc::new(sampler),
        ProxySelfGateConfig {
            cps_bucket_size: 50,
            cps_bucket_rate: 100,
            elu_critical: 0.8,
            ..Default::default()
        },
    );
    // The runner's sampler task, verbatim shape.
    let task = {
        let g = g.clone();
        let period = g.sampler_interval();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // skip the immediate first tick
            loop {
                tick.tick().await;
                g.sample();
            }
        })
    };

    // Calm to start: a fresh external INVITE is admitted.
    assert!(g.try_admit_external().is_admit());

    // Inject a pegged ELU and let several sampler ticks pull the EWMA above
    // 0.8 (alpha 0.2 needs a few ticks from 0 to cross the threshold).
    ctl.set_elu(1.0);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await; // let the spawned tick run its sample()
    }
    assert!(
        g.elu_ewma() > 0.8,
        "the running task must have driven the EWMA over critical, got {}",
        g.elu_ewma()
    );

    let d = g.try_admit_external();
    assert!(
        matches!(d, AdmitDecision::Reject { reason: ShedReason::IntakeAge, .. }),
        "a pegged ELU must shed the next external INVITE: {d:?}"
    );

    task.abort();
}
