use std::collections::HashMap;

use super::*;

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k| map.get(k).cloned()
}

#[test]
fn unset_variables_take_the_stated_defaults() {
    let s = LinkSettings::from_lookup(lookup(&[])).expect("defaults are valid");
    assert_eq!(s.connect_timeout, Duration::from_secs(2));
    assert_eq!(s.idle_timeout, Duration::from_secs(15));
    assert_eq!(s.noop_interval, Duration::from_secs(5));
    assert_eq!(s.send_stall, Duration::from_secs(15));
    assert_eq!(s.socket.keepalive_idle, Duration::from_secs(5));
    assert_eq!(s.socket.keepalive_interval, Duration::from_secs(2));
    assert_eq!(s.socket.keepalive_retries, 3);
    assert_eq!(s.socket.user_timeout, Duration::from_secs(15));
    assert_eq!(s.lookup_timeout, Duration::from_secs(2));
}

#[test]
fn every_bound_is_read_from_its_variable() {
    let s = LinkSettings::from_lookup(lookup(&[
        ("B2BUA_REPL_CONNECT_TIMEOUT_MS", "1500"),
        ("B2BUA_REPL_IDLE_TIMEOUT_MS", "9000"),
        ("B2BUA_REPL_NOOP_INTERVAL_MS", "3000"),
        ("B2BUA_REPL_SEND_STALL_MS", "8000"),
        ("B2BUA_REPL_KEEPALIVE_IDLE_MS", "4000"),
        ("B2BUA_REPL_KEEPALIVE_INTERVAL_MS", "1000"),
        ("B2BUA_REPL_KEEPALIVE_RETRIES", "5"),
        ("B2BUA_REPL_USER_TIMEOUT_MS", "12000"),
        ("B2BUA_REPL_LOOKUP_TIMEOUT_MS", "700"),
    ]))
    .expect("valid");
    let puller = s.puller();
    assert_eq!((puller.connect_timeout_ms, puller.idle_timeout_ms), (1500, 9000));
    assert_eq!(
        s.serve(),
        ServeConfig { noop_interval: Duration::from_secs(3), send_stall: Duration::from_secs(8) }
    );
    assert_eq!(
        s.socket,
        SocketSettings {
            keepalive_idle: Duration::from_secs(4),
            keepalive_interval: Duration::from_secs(1),
            keepalive_retries: 5,
            user_timeout: Duration::from_secs(12),
        }
    );
    assert_eq!(s.lookup_timeout, Duration::from_millis(700));
}

#[test]
fn a_bound_that_is_not_a_positive_integer_refuses_boot_naming_its_variable() {
    for key in [
        "B2BUA_REPL_CONNECT_TIMEOUT_MS",
        "B2BUA_REPL_IDLE_TIMEOUT_MS",
        "B2BUA_REPL_NOOP_INTERVAL_MS",
        "B2BUA_REPL_SEND_STALL_MS",
        "B2BUA_REPL_KEEPALIVE_IDLE_MS",
        "B2BUA_REPL_KEEPALIVE_INTERVAL_MS",
        "B2BUA_REPL_KEEPALIVE_RETRIES",
        "B2BUA_REPL_USER_TIMEOUT_MS",
        "B2BUA_REPL_LOOKUP_TIMEOUT_MS",
    ] {
        for bad in ["0", "-1", "soon"] {
            let e = LinkSettings::from_lookup(lookup(&[(key, bad)]))
                .expect_err("a non-positive bound refuses boot");
            assert!(e.contains(key), "{key}={bad}: msg was: {e}");
        }
    }
}

#[test]
fn an_idle_timeout_not_above_the_noop_interval_refuses_boot() {
    let e = LinkSettings::from_lookup(lookup(&[
        ("B2BUA_REPL_IDLE_TIMEOUT_MS", "5000"),
        ("B2BUA_REPL_NOOP_INTERVAL_MS", "5000"),
    ]))
    .expect_err("a quiet live stream would be left");
    assert!(e.contains("B2BUA_REPL_IDLE_TIMEOUT_MS"), "msg was: {e}");
}
