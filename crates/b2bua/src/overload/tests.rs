//! Signal-driven suite: header schema, EWMA feeding, the Tier-3 admission
//! gate, and the Prometheus render. Primitive-local tests live beside their
//! module (`bucket`, `sampler`, `admission`).

use super::*;
use std::sync::Arc;
use std::time::Duration;

/// The header value follows the `v=1` schema with elu, gc, adm fields.
/// Format-only assertion (the EWMAs may be 0 before the sampler fires), plus
/// the exact zero-state header a fresh signal publishes.
#[test]
fn header_value_follows_v1_schema() {
    let (sampler, _ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    let header = sig.x_overload_header_value();
    assert!(
        is_v1_schema(&header),
        "header {header:?} must match v=1; elu=<d>.<ddd>; gc=<d>.<ddd>; adm=<d>"
    );
    // And it must be exactly the zero-state header before any sample fires.
    assert_eq!(header, "v=1; elu=0.000; gc=0.000; adm=0");
}

/// `increment_non_emergency_admitted` advances the `adm` counter in the header.
#[test]
fn increment_non_emergency_admitted_advances_adm_in_header() {
    let (sampler, _ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    let before = parse_adm(&sig.x_overload_header_value());
    sig.increment_non_emergency_admitted();
    sig.increment_non_emergency_admitted();
    sig.increment_non_emergency_admitted();
    let after = parse_adm(&sig.x_overload_header_value());
    assert_eq!(after, before + 3);
    // The metrics surface mirrors the counter.
    assert_eq!(sig.metrics().non_emergency_admitted_total, before + 3);
}

/// `metrics().elu_ewma` and `gc_fraction_ewma` start at 0 before the sampler
/// fires. No `sample()` is called, so both EWMAs are still uninitialised.
#[test]
fn ewmas_start_at_zero_before_the_sampler_fires() {
    let (sampler, ctl) = simulated();
    // Even with a non-zero injected reading, no tick has fed the EWMA yet.
    ctl.set_elu(0.8);
    ctl.set_gc_fraction(0.2);
    let sig = OverloadSignal::new(Arc::new(sampler));
    assert_eq!(sig.metrics().elu_ewma, 0.0);
    assert_eq!(sig.metrics().gc_fraction_ewma, 0.0);
}

/// An injected sampler reading drives the EWMAs once `sample()` fires.
/// `sample()` is an explicit tick, so this pins the sample-to-EWMA half in
/// isolation with no clock at all; the *full*
/// injected-value → running-100ms-task → published-header loop (which the live
/// sampler alone cannot exercise, since its busy ratio reads ~0 under a paused
/// runtime) is closed end-to-end by
/// `injected_sampler_drives_published_elu_through_the_running_task`
/// (`b2bua-harness/tests/x_overload_signal.rs`), which injects this exact
/// `simulated()` sampler into a running `B2buaCore` via the
/// `spawn_with_overload` seam.
#[test]
fn load_sampler_injection_drives_elu_ewma_once_sampled() {
    let (sampler, ctl) = simulated();
    ctl.set_elu(0.8);
    ctl.set_gc_fraction(0.2);
    let sig = OverloadSignal::new(Arc::new(sampler));
    // One tick is enough for a fresh EWMA (first observe == the sample).
    sig.sample();
    assert!(sig.metrics().elu_ewma > 0.0);
    assert!(sig.metrics().gc_fraction_ewma > 0.0);
    // First observe seats the EWMA exactly at the sample.
    assert!((sig.metrics().elu_ewma - 0.8).abs() < 1e-9);
    assert!((sig.metrics().gc_fraction_ewma - 0.2).abs() < 1e-9);
    let header = sig.x_overload_header_value();
    assert!(is_v1_schema(&header), "header {header:?}");
    assert_eq!(header, "v=1; elu=0.800; gc=0.200; adm=0");
}

/// The EWMA smooths toward a sustained reading across repeated ticks (alpha
/// 0.2): after the first tick seats it, subsequent ticks pull it toward the
/// new value but never past it.
#[test]
fn ewma_smooths_across_repeated_samples() {
    let (sampler, ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    ctl.set_elu(1.0);
    sig.sample(); // seats at 1.0
    assert!((sig.metrics().elu_ewma - 1.0).abs() < 1e-9);
    ctl.set_elu(0.0);
    sig.sample(); // 0.2*0 + 0.8*1.0 = 0.8
    assert!((sig.metrics().elu_ewma - 0.8).abs() < 1e-9);
    sig.sample(); // 0.2*0 + 0.8*0.8 = 0.64
    assert!((sig.metrics().elu_ewma - 0.64).abs() < 1e-9);
}

// ── Tier-3 admission gate ───────────────────────────────────────────────────
//
// Where the bucket's time-based refill is exercised the test is `start_paused`
// and drives `tokio::time::advance`, since the bucket rides
// `tokio::time::Instant` (CLAUDE.md: behaviour rides `tokio::time` — no
// separate fake clock to keep in sync).

/// Build a signal whose admission gate uses the given bucket/threshold knobs,
/// over a simulated sampler whose ELU the returned control sets. The EWMA is
/// seated by one `sample()` so `should_admit`'s panic-ELU read sees a real value.
fn admission_sig(
    size: u32,
    rate: u32,
    panic_elu: f64,
) -> (OverloadSignal, SimulatedLoadControl) {
    let (sampler, ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    sig.configure_admission(&crate::config::B2buaConfig {
        cps_bucket_size: size,
        cps_bucket_rate: rate,
        overload_panic_elu_threshold: panic_elu,
        retry_after_base_sec: 5,
        ..Default::default()
    });
    (sig, ctl)
}

/// `configure_admission` seeds a full bucket at the configured capacity and a
/// non-emergency INVITE is admitted while tokens remain (`bucket_empty` only
/// once drained).
#[tokio::test(start_paused = true)]
async fn admits_until_the_cps_bucket_is_drained_then_503s_bucket_empty() {
    // Capacity 2, refill 0/s so the bucket can't top up between consumes.
    let (sig, _ctl) = admission_sig(2, 0, 1.0);
    // Two admits drain the burst capacity…
    assert_eq!(sig.should_admit(false), AdmitDecision::admitted());
    assert_eq!(sig.should_admit(false), AdmitDecision::admitted());
    // …the third finds the bucket empty → reject `bucket_empty`.
    let d = sig.should_admit(false);
    assert!(!d.admit);
    assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
    // rate 0 + empty → the 60 s Retry-After fallback.
    assert_eq!(d.retry_after_sec, 60);
    // The reject is tallied; no admit was counted for it.
    assert_eq!(sig.metrics().reject_bucket_empty_total, 1);
}

/// The bucket refills over wall time at `rate_per_sec` (lazy refill on the next
/// consume). Drained at capacity 1 / rate 1/s, a consume one second later
/// succeeds again — driven by `tokio::time::advance`, no real sleep.
#[tokio::test(start_paused = true)]
async fn the_bucket_refills_over_time() {
    let (sig, _ctl) = admission_sig(1, 1, 1.0);
    assert!(sig.should_admit(false).admit); // drains the lone token
    assert!(!sig.should_admit(false).admit); // empty immediately after
    // One second of refill at 1 token/s restores exactly one token.
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(sig.should_admit(false).admit, "a refilled token should admit");
}

/// Emergency callers ALWAYS admit and are NEVER counted on `adm`, but still
/// consume a token — so the bucket can go negative and a subsequent
/// non-emergency caller is shed.
#[tokio::test(start_paused = true)]
async fn emergency_always_admits_consumes_and_can_overdraft_the_bucket() {
    // Capacity 1, no refill: a single non-emergency token exists.
    let (sig, _ctl) = admission_sig(1, 0, 1.0);
    // Two emergency admits both succeed (the 2nd drives the level negative).
    assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
    assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
    // Emergency admits are NOT counted on `adm` (the caller skips the bump;
    // `should_admit` itself never touches the counter).
    assert_eq!(sig.metrics().non_emergency_admitted_total, 0);
    // The overdraft means the next NON-emergency caller finds the bucket empty.
    let d = sig.should_admit(false);
    assert!(!d.admit);
    assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
}

/// Once a token is consumed, an EWMA-ELU above the panic threshold sheds the
/// (non-emergency) call with `panic_elu` + the configured Retry-After base.
#[tokio::test(start_paused = true)]
async fn panic_elu_backstop_503s_after_the_token_is_consumed() {
    // Roomy bucket so the gate never trips on `bucket_empty`; panic at 0.75.
    let (sig, ctl) = admission_sig(1000, 0, 0.75);
    ctl.set_elu(0.9);
    sig.sample(); // seat the EWMA at 0.9 (> 0.75)
    let d = sig.should_admit(false);
    assert!(!d.admit);
    assert_eq!(d.reason, Some(AdmitReason::PanicElu));
    assert_eq!(d.retry_after_sec, 5); // retry_after_base_sec
    assert_eq!(sig.metrics().reject_panic_elu_total, 1);
    // A token WAS consumed before the panic check (spent in step 2 of
    // `should_admit`). With ELU back below the threshold the next call admits
    // normally.
    ctl.set_elu(0.0);
    sig.sample();
    sig.sample(); // pull the EWMA below 0.75
    assert!(sig.should_admit(false).admit);
}

/// The panic-ELU backstop is a NON-emergency control: an emergency caller is
/// admitted even when the worker's ELU is pegged (it bypasses both the bucket
/// gate's empty-check and the panic check — it only `consume_forced`s).
#[tokio::test(start_paused = true)]
async fn panic_elu_never_sheds_an_emergency_call() {
    let (sig, ctl) = admission_sig(1000, 0, 0.75);
    ctl.set_elu(1.0);
    sig.sample(); // EWMA pegged at 1.0
    assert_eq!(sig.should_admit(true), AdmitDecision::admitted());
    assert_eq!(sig.metrics().reject_panic_elu_total, 0);
}

/// `configure_admission` is what makes the operator's `B2buaConfig` knobs take
/// effect: a capacity-0 bucket sheds the very first non-emergency INVITE.
#[tokio::test(start_paused = true)]
async fn configure_admission_applies_the_config_capacity() {
    let (sig, _ctl) = admission_sig(0, 0, 1.0);
    let d = sig.should_admit(false);
    assert!(!d.admit, "a zero-capacity bucket admits nothing");
    assert_eq!(d.reason, Some(AdmitReason::BucketEmpty));
}

/// `increment_emergency_admitted` advances the emergency-admit counter
/// (sibling to the non-emergency `adm`), and it is NOT counted on `adm`.
#[test]
fn increment_emergency_admitted_advances_its_own_counter() {
    let (sampler, _ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    sig.increment_emergency_admitted();
    sig.increment_emergency_admitted();
    let m = sig.metrics();
    assert_eq!(m.emergency_admitted_total, 2);
    // Emergency admits never touch the non-emergency `adm` counter.
    assert_eq!(m.non_emergency_admitted_total, 0);
}

/// The Prometheus render carries every overload INPUT + DECISION series with
/// the right name/type after the counters/gauges advance.
#[tokio::test(start_paused = true)]
async fn prometheus_text_renders_inputs_and_decisions() {
    let (sig, ctl) = admission_sig(1, 0, 0.75);
    ctl.set_elu(0.42);
    ctl.set_gc_fraction(0.0);
    sig.sample(); // seat the ELU EWMA at 0.42
    // One non-emergency admit (drains the lone token), one emergency admit
    // (consume_forced → overdraft), then a non-emergency reject (bucket empty).
    assert!(sig.should_admit(false).admit);
    sig.increment_non_emergency_admitted();
    assert!(sig.should_admit(true).admit);
    sig.increment_emergency_admitted();
    assert!(!sig.should_admit(false).admit); // bucket_empty reject

    let txt = sig.prometheus_text();
    // admit_total = non_emergency(1) + emergency(1) = 2.
    assert!(txt.contains("b2bua_overload_admit_total 2"), "{txt}");
    assert!(txt.contains("# TYPE b2bua_overload_admit_total counter"));
    assert!(txt.contains("b2bua_overload_reject_total{reason=\"bucket_empty\"} 1"));
    assert!(txt.contains("b2bua_overload_reject_total{reason=\"panic_elu\"} 0"));
    assert!(txt.contains("b2bua_overload_non_emergency_admitted_total 1"));
    assert!(txt.contains("b2bua_emergency_admitted_total 1"));
    assert!(txt.contains("# TYPE b2bua_emergency_admitted_total counter"));
    assert!(txt.contains("# TYPE b2bua_overload_token_bucket_level gauge"));
    assert!(txt.contains("b2bua_overload_elu_ewma 0.42"));
    assert!(txt.contains("# TYPE b2bua_overload_elu_ewma gauge"));
    assert!(txt.contains("b2bua_overload_gc_fraction 0"));
}

// --- test-local header helpers ---------------------------------------------

/// `^v=1; elu=<d+>.<ddd>; gc=<d+>.<ddd>; adm=<d+>$` without a regex dep.
fn is_v1_schema(h: &str) -> bool {
    let rest = match h.strip_prefix("v=1; elu=") {
        Some(r) => r,
        None => return false,
    };
    let (elu, rest) = match rest.split_once("; gc=") {
        Some(p) => p,
        None => return false,
    };
    let (gc, adm) = match rest.split_once("; adm=") {
        Some(p) => p,
        None => return false,
    };
    is_fixed3(elu) && is_fixed3(gc) && !adm.is_empty() && adm.bytes().all(|b| b.is_ascii_digit())
}

/// `\d+\.\d{3}` — one or more integer digits, a dot, exactly three fractionals.
fn is_fixed3(s: &str) -> bool {
    let (int, frac) = match s.split_once('.') {
        Some(p) => p,
        None => return false,
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.len() == 3
        && frac.bytes().all(|b| b.is_ascii_digit())
}

fn parse_adm(h: &str) -> u64 {
    let idx = h.find("adm=").expect("no adm in header");
    h[idx + 4..].trim().parse().expect("adm not a number")
}
