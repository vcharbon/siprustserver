use super::*;
use load_shed::{simulated, SimulatedLoadControl};

#[test]
fn stub_always_admits() {
    let g = AlwaysAdmitGate;
    assert!(g.try_admit_external().is_admit());
    g.note_bypass(BypassKind::Emergency);
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
    ctl.set_elu(1.0);
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
    g.sample();
    assert_eq!(
        g.try_admit_external(),
        AdmitDecision::Reject { reason: ShedReason::IntakeAge, retry_after_sec: 1 }
    );
    assert_eq!(g.metrics().rejected_intake_age_total, 1);
    ctl.set_elu(0.0);
    g.sample();
    g.sample();
    assert!(g.try_admit_external().is_admit(), "the ELU reject must not have spent the token");
}

#[tokio::test(start_paused = true)]
async fn elu_exactly_at_critical_still_admits() {
    let (g, ctl) = gate(10, 0, 0.8);
    ctl.set_elu(0.8);
    g.sample();
    assert!(g.try_admit_external().is_admit(), "elu == critical is admit (strict >)");
}

/// With ELU calm, the gate admits until the CPS bucket drains, then sheds
/// with `proxy_overload_cps`.
/// With no refill rate, the refusal's Retry-After falls back to 60 s.
#[tokio::test(start_paused = true)]
async fn admits_until_the_cps_bucket_drains_then_503s_cps() {
    let (g, _ctl) = gate(2, 0, 0.8);
    assert!(g.try_admit_external().is_admit());
    assert!(g.try_admit_external().is_admit());
    let d = g.try_admit_external();
    assert_eq!(d, AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: 60 });
    assert_eq!(g.metrics().rejected_cps_total, 1);
    assert_eq!(g.metrics().external_admitted_total, 2);
}

/// The bucket refills over wall time at `rate_per_sec` (lazy refill on the
/// next consume), driven by `tokio::time::advance` — no real sleep.
#[tokio::test(start_paused = true)]
async fn the_cps_bucket_refills_over_time() {
    let (g, _ctl) = gate(1, 1, 0.8);
    assert!(g.try_admit_external().is_admit());
    assert!(!g.try_admit_external().is_admit());
    tokio::time::advance(Duration::from_secs(1)).await;
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
    assert!(!g.try_admit_external().is_admit());
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
    g.sample();
    assert!((g.elu_ewma() - 1.0).abs() < 1e-9);
    ctl.set_elu(0.0);
    g.sample();
    assert!((g.elu_ewma() - 0.8).abs() < 1e-9);
    g.sample();
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
    rec.record(100);
    assert!((s.elu() - 0.5).abs() < 1e-9, "250ms / 500ms critical = 0.5");
    rec.record(AGE_CRITICAL_MS * 4);
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

    for _ in 0..10 {
        g.sample();
    }
    assert!(g.try_admit_external().is_admit(), "a drained intake must admit again");
}

/// The runner's 100 ms `sample()` task, spawned over the `simulated()` sampler,
/// carries an injected ELU into the EWMA as the paused clock advances, and the
/// gate sheds the next external INVITE.
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
    let task = {
        let g = g.clone();
        let period = g.sampler_interval();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                g.sample();
            }
        })
    };

    assert!(g.try_admit_external().is_admit());

    ctl.set_elu(1.0);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
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

/// Admits over `span` from now, taking one call every millisecond.
async fn admits_over(g: &EluCpsGate, span: Duration) -> usize {
    let mut admitted = 0;
    for _ in 0..span.as_millis() {
        admitted += usize::from(g.try_admit_external().is_admit());
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    admitted
}

/// An intake ramp admits 20 % of the rate, from a bucket of 20 % of its size,
/// until the first external INVITE, however long none comes; then it rises
/// linearly to the whole over its span from that INVITE: the first second at
/// 1000 offers/s takes about 20 tokens of the bucket and 20 of refill; a
/// second past the span, the whole rate of 100 and the tokens the bucket held.
/// The gauge reads it.
#[tokio::test(start_paused = true)]
async fn an_intake_ramp_holds_its_start_share_until_offered_then_rises_to_the_whole_rate() {
    let (g, _ctl) = gate(100, 100, 0.8);
    g.begin_ramp();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(g.metrics().ramp_active, "no INVITE offered: the ramp waits");
    assert_eq!(g.metrics().cps_bucket_max, 20.0);
    let first = admits_over(&g, Duration::from_secs(1)).await;
    assert!((38..=44).contains(&first), "{first} admitted in the ramp's first second");
    tokio::time::advance(Duration::from_secs(28)).await;
    assert!(g.metrics().ramp_active, "the span runs from the first INVITE");
    tokio::time::advance(Duration::from_secs(1)).await;
    let after = admits_over(&g, Duration::from_secs(1)).await;
    assert!(after >= 120, "{after} admitted once the ramp ran out");
    assert!(!g.metrics().ramp_active);
    assert_eq!(g.metrics().cps_bucket_max, 100.0);
}

/// A ramp of zero, or one starting at the whole rate, does nothing.
#[tokio::test(start_paused = true)]
async fn a_ramp_of_zero_or_from_the_whole_rate_does_nothing() {
    let (sampler, _ctl) = simulated();
    let sampler = Arc::new(sampler);
    let idle = ProxySelfGateConfig::default();
    for config in [
        ProxySelfGateConfig { ramp: Duration::ZERO, ..idle },
        ProxySelfGateConfig { ramp_start_pct: 100.0, ..idle },
    ] {
        let g = EluCpsGate::new(sampler.clone(), config);
        g.begin_ramp();
        assert!(!g.metrics().ramp_active);
        assert_eq!(g.metrics().cps_bucket_max, f64::from(config.cps_bucket_size));
    }
}
