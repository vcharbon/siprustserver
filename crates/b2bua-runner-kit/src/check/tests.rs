//! The accept bounds' clauses (ADR-0045 item 5) are refused together in one
//! message naming every variable involved; a start bound and a sync cap that
//! do not fit the setup budget together are refused (ADR-0070).

use std::time::Duration;

use b2bua::repl::{PullerConfig, ServeConfig, WritePath};
use b2bua::B2buaConfig;

use super::ConfigCheck;

#[test]
fn a_start_bound_and_sync_cap_past_the_setup_budget_are_refused() {
    let write_path = WritePath::default();
    let mut check = ConfigCheck::new();
    check.start_and_sync_fit_the_budget(&B2buaConfig::default(), &write_path);
    assert!(check.into_result().is_ok(), "4000 + 100 ms fit the 5000 ms budget");

    let config = B2buaConfig { call_start_bound_ms: 4_900, ..B2buaConfig::default() };
    let mut check = ConfigCheck::new();
    check.start_and_sync_fit_the_budget(&config, &write_path);
    let refusal = check.into_result().expect_err("4900 + 100 ms do not fit 5000 ms");
    for named in [
        "B2BUA_CALL_START_BOUND_MS=4900",
        "B2BUA_REPL_SYNC_CAP_MS=100",
        "B2BUA_CALL_CONTROL_TIMEOUT_MS=5000",
    ] {
        assert!(refusal.contains(named), "{named} missing from: {refusal}");
    }
}

#[test]
fn an_open_timeout_under_the_idle_timeout_and_a_cap_under_four_are_named_in_one_refusal() {
    let puller = PullerConfig { idle_timeout_ms: 3000, ..PullerConfig::default() };
    let serve = ServeConfig {
        open_timeout: Duration::from_millis(2500),
        max_connections: 3,
        ..ServeConfig::default()
    };
    let mut check = ConfigCheck::new();
    check.accept_bounds(&serve, &puller);
    let refusal = check.into_result().expect_err("both clauses are broken");
    for named in [
        "2 violations",
        "B2BUA_REPL_OPEN_TIMEOUT_MS=2500",
        "B2BUA_REPL_IDLE_TIMEOUT_MS=3000",
        "B2BUA_REPL_MAX_CONNECTIONS=3",
    ] {
        assert!(refusal.contains(named), "{named} missing from: {refusal}");
    }

    let mut check = ConfigCheck::new();
    let serve =
        ServeConfig { open_timeout: Duration::from_millis(3000), max_connections: 4, ..serve };
    check.accept_bounds(&serve, &puller);
    assert!(check.into_result().is_ok(), "the bounds at their floors hold");
}
