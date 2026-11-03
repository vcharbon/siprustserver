//! The accept bounds' clauses (ADR-0045 item 5) are refused together in one
//! message naming every variable involved.

use std::time::Duration;

use b2bua::repl::{PullerConfig, ServeConfig};

use super::ConfigCheck;

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
