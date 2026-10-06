//! Signal-driven suite: header schema, EWMA feeding, the panic-ELU and bucket
//! rungs read from the signal, and the Prometheus render. The shared primitives are tested in
//! `load_shed`; the live sampler's tests live beside it (`sampler`).

use super::*;
use crate::admission::{first_refusal, Class, Refused, RouterReadings};
use crate::capacity::CapacityReading;
use crate::new_calls::Refusal;
use load_shed::{simulated, SimulatedLoadControl};
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

// ── The panic-ELU and bucket rungs ─────────────────────────────────────────
//
// Where the bucket's time-based refill is exercised the test is `start_paused`
// and drives `tokio::time::advance`, since the bucket rides
// `tokio::time::Instant` (CLAUDE.md: behaviour rides `tokio::time` — no
// separate fake clock to keep in sync).

/// The router's rungs for one new call ([`first_refusal`]), the capacity
/// and shed rungs admitting and the panic-ELU and bucket rungs read from
/// `sig`: the first refusal, or an admit that spends a token when one is
/// there.
pub(super) fn admit(sig: &OverloadSignal, emergency: bool) -> Result<(), Refused> {
    struct Signal<'a>(&'a OverloadSignal);
    impl RouterReadings for Signal<'_> {
        fn capacity(&self) -> CapacityReading {
            CapacityReading { limits: Default::default(), occupancy: Default::default(), rss: None }
        }
        fn at_threshold(&self) -> bool {
            false
        }
        fn panic_elu(&self) -> (f64, f64) {
            self.0.panic_elu()
        }
        fn token_wait_sec(&self) -> u32 {
            self.0.token_wait_sec()
        }
    }
    let class = if emergency { Class::Emergency } else { Class::Normal };
    let refused = first_refusal(class, &Signal(sig));
    match refused {
        Some(refused) => Err(refused),
        None => {
            sig.spend_token();
            Ok(())
        }
    }
}

/// The reason `admit` refused for, if it did.
fn reason(verdict: Result<(), Refused>) -> Option<Refusal> {
    verdict.err().map(|r| r.reason)
}

/// Build a signal whose rungs use the given bucket/threshold knobs, over a
/// simulated sampler whose ELU the returned control sets.
fn admission_sig(size: u32, rate: u32, panic_elu: f64) -> (OverloadSignal, SimulatedLoadControl) {
    let (sampler, ctl) = simulated();
    let sig = OverloadSignal::new(Arc::new(sampler));
    sig.configure_admission(&crate::config::B2buaConfig {
        cps_bucket_size: size,
        cps_bucket_rate: rate,
        overload_panic_elu_threshold: panic_elu,
        ..Default::default()
    });
    (sig, ctl)
}

/// `configure_admission` seeds a full bucket at the configured capacity and a
/// normal call is admitted while tokens remain, then refused `bucket_empty`
/// with the time to the next token.
#[tokio::test(start_paused = true)]
async fn admits_until_the_cps_bucket_is_drained_then_refuses_bucket_empty() {
    // Capacity 2, refill 0/s so the bucket can't top up between consumes.
    let (sig, _ctl) = admission_sig(2, 0, 1.0);
    assert_eq!(admit(&sig, false), Ok(()));
    assert_eq!(admit(&sig, false), Ok(()));
    let refused = admit(&sig, false).expect_err("the third finds the bucket empty");
    assert_eq!(refused.reason, Refusal::BucketEmpty);
    // rate 0 + empty → the 60 s fallback.
    assert_eq!(refused.not_before_sec, 60);
}

/// The bucket refills over time at `rate_per_sec` (lazy refill on the next
/// peek). Drained at capacity 1 / rate 1/s, a call one second later is
/// admitted again — driven by `tokio::time::advance`, no real sleep.
#[tokio::test(start_paused = true)]
async fn the_bucket_refills_over_time() {
    let (sig, _ctl) = admission_sig(1, 1, 1.0);
    assert!(admit(&sig, false).is_ok()); // drains the lone token
    assert!(admit(&sig, false).is_err()); // empty immediately after
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(admit(&sig, false).is_ok(), "a refilled token should admit");
}

/// An emergency call passes both rungs and is NEVER counted on `adm`. It
/// spends a token when one is there, so it shares the rate with normal calls,
/// and passes without one when the bucket is empty, leaving no debt.
#[tokio::test(start_paused = true)]
async fn emergency_always_admits_and_spends_only_a_token_that_is_there() {
    let (sig, _ctl) = admission_sig(2, 0, 1.0);
    assert_eq!(admit(&sig, true), Ok(()));
    assert_eq!(sig.metrics().token_bucket_level, 1.0);
    assert_eq!(admit(&sig, false), Ok(()));
    assert_eq!(admit(&sig, true), Ok(()));
    assert_eq!(admit(&sig, true), Ok(()));
    assert_eq!(sig.metrics().token_bucket_level, 0.0);
    assert_eq!(sig.metrics().non_emergency_admitted_total, 0);
    assert_eq!(reason(admit(&sig, false)), Some(Refusal::BucketEmpty));
}

/// An emergency surge above the refill rate leaves the bucket empty, not in
/// debt: once it ends, a normal INVITE waits at most one refill interval
/// (`1 / rate`), its refusal says so, and the level metric reads the refill.
#[tokio::test(start_paused = true)]
async fn an_emergency_surge_leaves_no_debt_behind() {
    let (sig, _ctl) = admission_sig(100, 100, 1.0);
    // 60 s of emergency INVITEs at 500/s (five every 10 ms), five times the rate.
    for step in 0..6000 {
        if step > 0 {
            tokio::time::advance(Duration::from_millis(10)).await;
        }
        for _ in 0..5 {
            assert_eq!(admit(&sig, true), Ok(()));
        }
    }
    let level = sig.metrics().token_bucket_level;
    assert!((0.0..1.0).contains(&level), "level {level}");
    let refused = admit(&sig, false).expect_err("the bucket is empty");
    assert_eq!(refused.reason, Refusal::BucketEmpty);
    assert_eq!(refused.not_before_sec, 1, "one token away at 100/s");
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_eq!(admit(&sig, false), Ok(()));
    tokio::time::advance(Duration::from_millis(500)).await;
    let refilled = sig.metrics().token_bucket_level;
    assert!((refilled - 50.0).abs() < 1e-6, "level {refilled}");
}

/// An EWMA-ELU above the panic threshold refuses a normal call `panic_elu`
/// before the bucket is read, and spends no token (ADR-0037).
#[tokio::test(start_paused = true)]
async fn a_panic_elu_refusal_spends_no_token() {
    let (sig, ctl) = admission_sig(1, 0, 0.75);
    ctl.set_elu(0.9);
    sig.sample(); // seat the EWMA at 0.9 (> 0.75)
    let refused = admit(&sig, false).expect_err("above the backstop");
    assert_eq!(refused.reason, Refusal::PanicElu);
    assert_eq!(refused.not_before_sec, 0, "the configured base");
    assert_eq!(sig.metrics().token_bucket_level, 1.0, "the token stays");
    ctl.set_elu(0.0);
    sig.sample();
    sig.sample(); // pull the EWMA below 0.75
    assert_eq!(admit(&sig, false), Ok(()), "the token is still there");
    assert_eq!(sig.metrics().token_bucket_level, 0.0);
}

/// The panic-ELU backstop refuses before the bucket: with both above their
/// limit the refusal names the backstop.
#[tokio::test(start_paused = true)]
async fn the_panic_backstop_is_judged_before_the_bucket() {
    let (sig, ctl) = admission_sig(0, 0, 0.75);
    ctl.set_elu(0.9);
    sig.sample();
    assert_eq!(reason(admit(&sig, false)), Some(Refusal::PanicElu));
}

/// The panic-ELU backstop is a normal-call control: an emergency caller is
/// admitted even when the worker's ELU is pegged.
#[tokio::test(start_paused = true)]
async fn panic_elu_never_sheds_an_emergency_call() {
    let (sig, ctl) = admission_sig(1000, 0, 0.75);
    ctl.set_elu(1.0);
    sig.sample(); // EWMA pegged at 1.0
    assert_eq!(admit(&sig, true), Ok(()));
}

/// `configure_admission` is what makes the operator's `B2buaConfig` knobs take
/// effect: a capacity-0 bucket refuses the very first normal INVITE.
#[tokio::test(start_paused = true)]
async fn configure_admission_applies_the_config_capacity() {
    let (sig, _ctl) = admission_sig(0, 0, 1.0);
    assert_eq!(reason(admit(&sig, false)), Some(Refusal::BucketEmpty));
}

/// The Prometheus render carries every rung input with the right name and
/// type, and no admit or refusal count: those are new-call counts.
#[tokio::test(start_paused = true)]
async fn prometheus_text_renders_the_rung_inputs() {
    let (sig, ctl) = admission_sig(1, 0, 0.75);
    ctl.set_elu(0.42);
    ctl.set_gc_fraction(0.0);
    sig.sample(); // seat the ELU EWMA at 0.42
    assert!(admit(&sig, false).is_ok()); // drains the lone token
    assert!(admit(&sig, false).is_err()); // bucket_empty

    let txt = sig.prometheus_text();
    assert!(!txt.contains("admit"), "an admit is a new-call count: {txt}");
    assert!(!txt.contains("reject"), "a refusal is a new-call count: {txt}");
    assert!(txt.contains("b2bua_overload_token_bucket_level 0"), "{txt}");
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
