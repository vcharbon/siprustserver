//! Port of `tests/sip-front-proxy/config-validation.test.ts`.
//!
//! The validator is a pure boot-time guard (no clock, no `tokio`, no I/O), so
//! these are plain `#[test]`s driven with literal config values exactly as the
//! TS `vitest` suite is — no paused runtime needed (default lane, sub-ms).
use super::*;

/// The Helm chart's production-shape probe timings. Acts as the canonical
/// "real deployment" fixture: cycle = 2000 + 1500 = 3500 ms. If a test breaks
/// because the chart changes its defaults, that is the signal to revisit the
/// calibration (mirrors the TS `HELM_PROBE`).
const HELM_PROBE: ProbeTimingConfig = ProbeTimingConfig { interval_ms: 2000, timeout_ms: 1500 };

/// `defaultWorkerLoadObserverConfig` + the given overrides (TS `withObserver`).
fn with_observer(f: impl FnOnce(&mut LoadObserverConfig)) -> LoadObserverConfig {
    let mut cfg = LoadObserverConfig::default();
    f(&mut cfg);
    cfg
}

fn msgs(probe: ProbeTimingConfig, observer: &LoadObserverConfig) -> Vec<String> {
    validate_config(probe, observer).err().unwrap_or_default()
}

// ── happy path ─────────────────────────────────────────────────────────

#[test]
fn shipped_defaults_pass_against_the_deployed_helm_probe_timings() {
    assert!(validate_config(HELM_PROBE, &LoadObserverConfig::default()).is_ok());
}

#[test]
fn happy_path_against_the_in_code_defaults_interval_1000() {
    let probe = ProbeTimingConfig { interval_ms: 1000, timeout_ms: 1500 };
    assert!(validate_config(probe, &LoadObserverConfig::default()).is_ok());
}

// ── primary invariant: payload_stale_ms vs probe cycle ──────────────────

#[test]
fn the_exact_rca_misconfiguration_is_rejected() {
    // Helm 3500 ms cycle, observer 3000 ms stale.
    let m = msgs(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000)).join("\n");
    assert!(m.contains("payload_stale_ms"), "{m}");
    assert!(m.contains("3500 ms"), "cycle reported: {m}");
    assert!(m.contains("7000 ms"), "2x cycle reported: {m}");
}

#[test]
fn rejects_exactly_one_cycle_margin() {
    // probe_cycle = 3500, stale = 3500 → still rejected (need 2×).
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

// ── value-positivity guards ─────────────────────────────────────────────
// NOTE: the TS NaN / negative-interval / negative-timeout cases are
// unrepresentable here — probe timings are `u64`, so only the `== 0` arm of
// the TS positivity guard is reachable for interval/timeout. payload_stale_ms
// is `i64`, so its negative case IS exercised below.

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

// ── cap-band sanity ─────────────────────────────────────────────────────

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

// ── ELU band ordering and hysteresis (delegates to validate_bands) ──────

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

#[test]
fn rejects_hysteresis_wider_than_a_band_gap() {
    // hard − soft = 0.6 − 0.4 = 0.2; hysteresis 0.25 traps the controller.
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = 0.25)).is_err());
}

#[test]
fn rejects_negative_hysteresis() {
    assert!(validate_config(HELM_PROBE, &with_observer(|c| c.band_hysteresis = -0.01)).is_err());
}

// ── cooldown sanity ─────────────────────────────────────────────────────

#[test]
fn rejects_cooldown_shorter_than_one_probe_cycle() {
    // cooldown = 1 × 1000 = 1000 ms < cycle 3500 ms.
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
    // cooldown = 4 × 1000 = 4000 ms >= 3500 ms cycle.
    assert!(validate_config(
        HELM_PROBE,
        &with_observer(|c| {
            c.aimd_cooldown_ticks = 4.0;
            c.options_interval_ms = 1000;
        }),
    )
    .is_ok());
}

// ── multiple simultaneous violations are all reported ───────────────────

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
    // Operators must see ALL problems on the first boot attempt, not play
    // whack-a-mole one error at a time.
    assert!(violations.len() > 1, "expected >1 violation, got {violations:?}");
}

// ── assert_valid_config — panics on violation ───────────────────────────

#[test]
#[should_panic(expected = "refusing to start")]
fn assert_valid_config_panics_with_the_refuse_to_start_header() {
    assert_valid_config(HELM_PROBE, &with_observer(|c| c.payload_stale_ms = 3000));
}

#[test]
fn assert_valid_config_does_not_panic_on_a_valid_config() {
    // No `#[should_panic]` ⇒ a panic here fails the test.
    assert_valid_config(HELM_PROBE, &LoadObserverConfig::default());
}

// ── parse_ext_face_cidrs — dual-face boot rule ───────────────────────────

#[test]
fn single_face_ignores_cidrs() {
    assert_eq!(parse_ext_face_cidrs(false, None).unwrap(), None);
    assert_eq!(parse_ext_face_cidrs(false, Some("10.0.0.0/8")).unwrap(), None);
}

#[test]
fn dual_face_requires_cidrs() {
    let e = parse_ext_face_cidrs(true, None).unwrap_err();
    assert!(e.contains("PROXY_FACE_INT_CIDRS is REQUIRED"), "{e}");
    let e = parse_ext_face_cidrs(true, Some("  ")).unwrap_err();
    assert!(e.contains("REQUIRED"), "blank counts as missing: {e}");
}

#[test]
fn dual_face_rejects_unparseable_cidrs() {
    let e = parse_ext_face_cidrs(true, Some("10.0.0.0/8,bogus")).unwrap_err();
    assert!(e.contains("bad PROXY_FACE_INT_CIDRS"), "{e}");
}

#[test]
fn dual_face_parses_a_valid_list() {
    let f = parse_ext_face_cidrs(true, Some("10.244.0.0/16,172.20.0.0/16")).unwrap().unwrap();
    assert!(f.contains_ip("10.244.1.2".parse().unwrap()));
    assert!(!f.contains_ip("192.168.60.9".parse().unwrap()));
}

// ── load_observer_cfg_from_env — LB_* operator overrides ─────────────────
//
// The process env is global, so these serialise behind one mutex and clean up
// every var they touch. They never spawn a runtime — pure CPU, default lane.

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const LB_VARS: &[&str] = &[
    "LB_ELU_SOFT",
    "LB_ELU_HARD",
    "LB_ELU_CRITICAL",
    "LB_BAND_HYSTERESIS",
    "LB_AIMD_DECREASE_FACTOR",
    "LB_AIMD_INCREASE_STEP_CPS",
    "LB_CAP_CEILING_CPS",
    "LB_CAP_FLOOR_CPS",
];

fn clear_lb_vars() {
    for k in LB_VARS {
        env::remove_var(k);
    }
}

/// With no `LB_*` vars set, the config is the shipped default verbatim.
#[test]
fn env_overrides_absent_keep_the_shipped_defaults() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_lb_vars();
    let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
    let def = LoadObserverConfig::default();
    assert_eq!(cfg.elu_soft, def.elu_soft);
    assert_eq!(cfg.elu_hard, def.elu_hard);
    assert_eq!(cfg.elu_critical, def.elu_critical);
    assert_eq!(cfg.cap_ceiling_cps, def.cap_ceiling_cps);
}

/// Each present, parseable `LB_*` var replaces exactly its field; a coherent
/// override set still passes the boot preflight.
#[test]
fn env_overrides_present_replace_fields_and_stay_coherent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_lb_vars();
    env::set_var("LB_ELU_SOFT", "0.3");
    env::set_var("LB_ELU_HARD", "0.45");
    env::set_var("LB_ELU_CRITICAL", "0.6");
    env::set_var("LB_BAND_HYSTERESIS", "0.05");
    env::set_var("LB_AIMD_DECREASE_FACTOR", "0.4");
    env::set_var("LB_CAP_CEILING_CPS", "150");
    env::set_var("LB_CAP_FLOOR_CPS", "2");
    let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
    clear_lb_vars();
    assert_eq!(cfg.elu_soft, 0.3);
    assert_eq!(cfg.elu_hard, 0.45);
    assert_eq!(cfg.elu_critical, 0.6);
    assert_eq!(cfg.aimd_decrease_factor, 0.4);
    assert_eq!(cfg.cap_ceiling_cps, 150.0);
    assert_eq!(cfg.cap_floor_cps, 2.0);
    // The result is coherent → the preflight accepts it.
    assert_valid_config(HELM_PROBE, &cfg);
}

/// A malformed value is IGNORED (the default field is kept) — never aborts.
#[test]
fn env_overrides_malformed_value_is_ignored() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_lb_vars();
    env::set_var("LB_ELU_CRITICAL", "not-a-number");
    let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
    clear_lb_vars();
    assert_eq!(cfg.elu_critical, LoadObserverConfig::default().elu_critical);
}

/// An INCOHERENT override set (reversed bands) is NOT silently accepted: it
/// applies, then the boot preflight refuses to start.
#[test]
#[should_panic(expected = "refusing to start")]
fn env_overrides_incoherent_set_fails_the_preflight() {
    let _g = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_lb_vars();
    // elu_critical < elu_hard < elu_soft — fully reversed band ordering.
    env::set_var("LB_ELU_SOFT", "0.8");
    env::set_var("LB_ELU_HARD", "0.5");
    env::set_var("LB_ELU_CRITICAL", "0.3");
    let cfg = load_observer_cfg_from_env(LoadObserverConfig::default());
    clear_lb_vars();
    assert_valid_config(HELM_PROBE, &cfg); // panics: refusing to start
}

// ── bind_with_retry — bounded EADDRINUSE wait ───────────────────────────
//
// Real sockets ⇒ real clock (docs/testing/test-clock.md forbids pausing
// tokio time under real socket readiness). Intervals are test-short
// (10 ms / 300 ms) so the whole group stays sub-second — default lane.

const TEST_INTERVAL: Duration = Duration::from_millis(10);
const TEST_DEADLINE: Duration = Duration::from_millis(300);

fn tcp_in_use(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AddrInUse
}

/// (a) The holder releases the port mid-retry: the bind lands well before
/// the deadline instead of the process dying on the first attempt.
#[tokio::test]
async fn bind_retry_succeeds_once_the_holder_releases() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = holder.local_addr().unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(holder);
    });
    let bound = bind_with_retry("test", addr, TEST_INTERVAL, TEST_DEADLINE, tcp_in_use, || {
        tokio::net::TcpListener::bind(addr)
    })
    .await
    .expect("retry must win once the holder releases");
    assert_eq!(bound.local_addr().unwrap(), addr);
    release.await.unwrap();
}

/// (b) The holder never releases: the deadline expires and the error names
/// the contested address. Exercises the UDP side end-to-end — the real
/// network's `BindError` classifies EADDRINUSE structurally.
#[tokio::test]
async fn bind_retry_gives_up_at_the_deadline_naming_the_addr() {
    let holder = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = holder.local_addr().unwrap();
    let net = RealSignalingNetwork::new();
    let err = bind_with_retry(
        "test",
        addr,
        TEST_INTERVAL,
        TEST_DEADLINE,
        |e: &BindError| e.is_addr_in_use(),
        || async move { net.bind_udp(BindUdpOpts::new(addr, 8)).await },
    )
    .await;
    // Manual unwrap of the Err arm — `Box<dyn UdpEndpoint>` is not Debug.
    let err = match err {
        Ok(_) => panic!("the addr stays held — the deadline must expire"),
        Err(e) => e,
    };
    assert!(err.contains(&addr.to_string()), "error must name the addr: {err}");
    assert!(err.contains("still in use"), "{err}");
    drop(holder);
}

/// A non-addr-in-use failure is NOT retried — it surfaces immediately
/// (still naming the address) so a genuine misconfiguration fails fast.
#[tokio::test]
async fn bind_retry_fails_fast_on_a_non_in_use_error() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = holder.local_addr().unwrap();
    let started = std::time::Instant::now();
    let err = bind_with_retry(
        "test",
        addr,
        TEST_INTERVAL,
        TEST_DEADLINE,
        |_: &std::io::Error| false,
        || tokio::net::TcpListener::bind(addr),
    )
    .await
    .expect_err("a non-retryable error must surface at once");
    assert!(started.elapsed() < TEST_DEADLINE, "must not wait out the deadline");
    assert!(err.contains(&addr.to_string()) && err.contains("failed"), "{err}");
}
