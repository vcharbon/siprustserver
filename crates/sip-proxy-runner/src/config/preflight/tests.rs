use super::*;
use sip_proxy::load_observer::LoadObserverConfig;

const HELM_PROBE: ProbeTimingConfig = ProbeTimingConfig { interval_ms: 2000, timeout_ms: 1500 };

fn with_observer(f: impl FnOnce(&mut LoadObserverConfig)) -> LoadObserverConfig {
    let mut cfg = LoadObserverConfig::default();
    f(&mut cfg);
    cfg
}

fn msgs(probe: ProbeTimingConfig, observer: &LoadObserverConfig) -> Vec<String> {
    validate_config(probe, observer).err().unwrap_or_default()
}

#[test]
fn shipped_defaults_pass_against_the_deployed_helm_probe_timings() {
    assert!(validate_config(HELM_PROBE, &LoadObserverConfig::default()).is_ok());
}

#[test]
fn happy_path_against_the_in_code_defaults_interval_1000() {
    let probe = ProbeTimingConfig { interval_ms: 1000, timeout_ms: 1500 };
    assert!(validate_config(probe, &LoadObserverConfig::default()).is_ok());
}

#[test]
fn the_exact_rca_misconfiguration_is_rejected() {
    let m = msgs(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000)).join("\n");
    assert!(m.contains("payload_stale_ms"), "{m}");
    assert!(m.contains("3500 ms"), "cycle reported: {m}");
    assert!(m.contains("7000 ms"), "2x cycle reported: {m}");
}

/// A stale bound of exactly one probe cycle is refused: it takes two.
#[test]
fn rejects_exactly_one_cycle_margin() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3500)).is_err());
}

#[test]
fn accepts_exactly_2x_cycle_boundary() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 7000)).is_ok());
}

#[test]
fn accepts_the_shipped_default_of_8000_ms() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 8000)).is_ok());
}

#[test]
fn rejects_interval_ms_zero() {
    let probe = ProbeTimingConfig { interval_ms: 0, timeout_ms: 1500 };
    assert!(validate_config(probe, &LoadObserverConfig::default()).is_err());
}

#[test]
fn rejects_timeout_ms_zero() {
    let probe = ProbeTimingConfig { interval_ms: 2000, timeout_ms: 0 };
    assert!(validate_config(probe, &LoadObserverConfig::default()).is_err());
}

#[test]
fn rejects_payload_stale_ms_zero() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 0)).is_err());
}

#[test]
fn rejects_payload_stale_ms_negative() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = -1)).is_err());
}

#[test]
fn rejects_cap_floor_gt_cap_initial() {
    let m = msgs(
        HELM_PROBE,
        &with_observer(|c| {
            c.cap_floor_cps = 50.0;
            c.cap_initial_cps = 30.0;
        }),
    )
    .join("\n");
    assert!(m.contains("cap_floor_cps"), "{m}");
}

#[test]
fn rejects_cap_initial_gt_cap_ceiling() {
    assert!(validate_config(
        HELM_PROBE,
        &with_observer(|c| {
            c.cap_initial_cps = 500.0;
            c.cap_ceiling_cps = 200.0;
        }),
    )
    .is_err());
}

#[test]
fn rejects_cap_floor_zero() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.cap_floor_cps = 0.0)).is_err());
}

#[test]
fn rejects_elu_soft_ge_elu_hard() {
    assert!(validate_config(
        HELM_PROBE,
        &with_observer(|c| {
            c.elu_soft = 0.6;
            c.elu_hard = 0.6;
        }),
    )
    .is_err());
}

#[test]
fn rejects_elu_hard_ge_elu_critical() {
    assert!(validate_config(
        HELM_PROBE,
        &with_observer(|c| {
            c.elu_hard = 0.75;
            c.elu_critical = 0.75;
        }),
    )
    .is_err());
}

/// A hysteresis of 0.25 is wider than the default soft-to-hard gap (0.6 − 0.4)
/// and would hold the controller in one band.
#[test]
fn rejects_hysteresis_wider_than_a_band_gap() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = 0.25)).is_err());
}

#[test]
fn rejects_negative_hysteresis() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = -0.01)).is_err());
}

#[test]
fn rejects_cooldown_shorter_than_one_probe_cycle() {
    let m = msgs(
        HELM_PROBE,
        &with_observer(|c| {
            c.aimd_cooldown_ticks = 1.0;
            c.options_interval_ms = 1000;
        }),
    )
    .join("\n");
    assert!(m.contains("cooldown"), "{m}");
}

#[test]
fn accepts_cooldown_ge_one_probe_cycle() {
    assert!(validate_config(
        HELM_PROBE,
        &with_observer(|c| {
            c.aimd_cooldown_ticks = 4.0;
            c.options_interval_ms = 1000;
        }),
    )
    .is_ok());
}

#[test]
fn invalid_probe_and_invalid_observer_both_surface() {
    let probe = ProbeTimingConfig { interval_ms: 0, timeout_ms: 1500 };
    let violations = msgs(
        probe,
        &with_observer(|c| {
            c.payload_stale_ms = 100;
            c.cap_floor_cps = 99.0;
            c.cap_initial_cps = 30.0;
        }),
    );
    assert!(violations.len() > 1, "expected >1 violation, got {violations:?}");
}

#[test]
fn the_refusal_names_every_violation_under_its_header() {
    let violations = msgs(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000));
    let text = refusal(&violations);
    assert!(text.starts_with("sip-front-proxy: refusing to start"), "{text}");
    for v in &violations {
        assert!(text.contains(&format!("  - {v}")), "{text}");
    }
}
