//! The call limiter client's env grammar (ADR-0040).
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `LIMITER_URL` | the limiter, `[http://]host:port[/]` | unset: no limiter |
//! | `LIMITER_TIMEOUT_MS` | the budget of one admit (fail-open past it) and health probe | 150 |
//! | `LIMITER_REFRESH_TIMEOUT_MS` | the budget of one refresh request | 2000 |
//! | `LIMITER_REFRESH_SECONDS` | how often a counted call extends its lease | 40 |
//! | `LIMITER_REFRESH_BATCH_MS` | the refresh tick: a refresh due leaves within it, batched | 1000 |
//! | `LIMITER_REFRESH_BATCH_MAX` | most keys one refresh request carries | 1000 |
//! | `LIMITER_RELEASE_TIMEOUT_MS` | the budget of one release request | 2000 |
//! | `LIMITER_RELEASE_QUEUE_CAP` | most releases the queue holds | 100000 |
//! | `LIMITER_*` of [`BackendConfig`] (breaker, lookup, transport) | the limiter client's tuning | the backend defaults, `LIMITER_MAX_IN_FLIGHT` 64, `LIMITER_HEALTH_PATH` `/v1/health`, `LIMITER_BREAKER_PROBE_TIMEOUT_MS` the admit budget |
//!
//! The lease is the limiter's own: its answers state it, and the worker
//! gives up a queued release or a refresh due by the lease it last learnt
//! (the limiter's default, 120 s, before any answer).
//!
//! Unset or blank takes the default; a value that is not a positive integer
//! refuses boot, and so does `LIMITER_BREAKER_FAILURES`, which names no knob.
//! A `LIMITER_URL` that is not `host:port` over plain HTTP (an
//! other scheme, no port, a path) refuses boot; a well-formed name that does
//! not resolve at boot starts the breaker open (ADR-0040 decision 10). The
//! breaker and the client follow ADR-0043.

use http_net::BackendConfig;

use crate::stated;

/// The limiter client's settings, as the env states them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LimiterEnv {
    pub timeout_ms: u64,
    pub refresh_timeout_ms: u64,
    pub refresh_sec: i64,
    pub refresh_batch_ms: u64,
    pub refresh_batch_max: usize,
    pub release_timeout_ms: u64,
    pub queue_cap: usize,
    pub backend: BackendConfig,
}

/// The limiter's `host:port` `LIMITER_URL` states: `None` when unset or
/// blank, an `Err` naming it when malformed ([`crate::backend_url`]).
pub(crate) fn limiter_url_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    crate::backend_url::backend_url_from_lookup("LIMITER_URL", lookup)
}

/// The settings `lookup` states. A value that is not a positive integer is
/// an `Err` naming it.
pub(crate) fn limiter_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<LimiterEnv, String> {
    let positive = |key: &str, default: u64| -> Result<u64, String> {
        match stated(lookup(key)) {
            None => Ok(default),
            Some(v) => match v.trim().parse::<u64>() {
                Ok(0) => Err(format!("{key}=0: must be positive")),
                Ok(n) => Ok(n),
                Err(e) => Err(format!("{key}={v:?}: {e}")),
            },
        }
    };
    let seconds = |key: &str, default: u64| -> Result<i64, String> {
        let n = positive(key, default)?;
        i64::try_from(n).map_err(|_| format!("{key}={n}: too large"))
    };
    let count = |n: u64| usize::try_from(n).map_err(|_| format!("{n}: past this platform"));
    if let Some(v) = stated(lookup("LIMITER_BREAKER_FAILURES")) {
        return Err(format!(
            "LIMITER_BREAKER_FAILURES={v:?} is not a knob: the breaker opens on \
             LIMITER_BREAKER_REQUESTS in LIMITER_BREAKER_WINDOW_MS (ADR-0043)"
        ));
    }
    let timeout_ms = positive("LIMITER_TIMEOUT_MS", 150)?;
    let defaults = BackendConfig::new(
        b2bua::limiter::http::MAX_IN_FLIGHT,
        std::time::Duration::from_millis(timeout_ms),
        b2bua::limiter::http::HEALTH_PATH,
    );
    Ok(LimiterEnv {
        timeout_ms,
        refresh_timeout_ms: positive("LIMITER_REFRESH_TIMEOUT_MS", 2_000)?,
        refresh_sec: seconds("LIMITER_REFRESH_SECONDS", 40)?,
        refresh_batch_ms: positive("LIMITER_REFRESH_BATCH_MS", 1_000)?,
        refresh_batch_max: count(positive("LIMITER_REFRESH_BATCH_MAX", 1_000)?)?,
        release_timeout_ms: positive("LIMITER_RELEASE_TIMEOUT_MS", 2_000)?,
        queue_cap: count(positive("LIMITER_RELEASE_QUEUE_CAP", 100_000)?)?,
        backend: BackendConfig::from_lookup("LIMITER", &defaults, &lookup)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const KEYS: [&str; 13] = [
        "LIMITER_TIMEOUT_MS",
        "LIMITER_REFRESH_TIMEOUT_MS",
        "LIMITER_REFRESH_SECONDS",
        "LIMITER_REFRESH_BATCH_MS",
        "LIMITER_REFRESH_BATCH_MAX",
        "LIMITER_RELEASE_TIMEOUT_MS",
        "LIMITER_RELEASE_QUEUE_CAP",
        "LIMITER_BREAKER_REQUESTS",
        "LIMITER_BREAKER_WINDOW_MS",
        "LIMITER_BREAKER_PROBE_MS",
        "LIMITER_CONNECT_TIMEOUT_MS",
        "LIMITER_MAX_IN_FLIGHT",
        "LIMITER_ROTATION_MS",
    ];

    fn from(pairs: &[(&str, &str)]) -> Result<LimiterEnv, String> {
        let pairs: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        limiter_from_lookup(|key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }

    #[test]
    fn unset_takes_the_defaults() {
        let env = from(&[]).unwrap();
        assert_eq!(
            env,
            LimiterEnv {
                timeout_ms: 150,
                refresh_timeout_ms: 2_000,
                refresh_sec: 40,
                refresh_batch_ms: 1_000,
                refresh_batch_max: 1_000,
                release_timeout_ms: 2_000,
                queue_cap: 100_000,
                backend: BackendConfig::new(64, Duration::from_millis(150), "/v1/health"),
            }
        );
    }

    #[test]
    fn stated_values_are_taken() {
        let env = from(&[
            ("LIMITER_TIMEOUT_MS", "100"),
            ("LIMITER_REFRESH_TIMEOUT_MS", "900"),
            ("LIMITER_REFRESH_SECONDS", "20"),
            ("LIMITER_REFRESH_BATCH_MS", "500"),
            ("LIMITER_REFRESH_BATCH_MAX", "50"),
            ("LIMITER_RELEASE_TIMEOUT_MS", "500"),
            ("LIMITER_RELEASE_QUEUE_CAP", "10"),
            ("LIMITER_BREAKER_REQUESTS", "5"),
            ("LIMITER_BREAKER_WINDOW_MS", "3000"),
            ("LIMITER_BREAKER_PROBE_MS", "250"),
            ("LIMITER_MAX_IN_FLIGHT", "16"),
            ("LIMITER_ROTATION_MS", "30000"),
        ])
        .unwrap();
        assert_eq!(
            env,
            LimiterEnv {
                timeout_ms: 100,
                refresh_timeout_ms: 900,
                refresh_sec: 20,
                refresh_batch_ms: 500,
                refresh_batch_max: 50,
                release_timeout_ms: 500,
                queue_cap: 10,
                backend: {
                    let mut b = BackendConfig::new(16, Duration::from_millis(100), "/v1/health");
                    b.breaker.min_sent = 5;
                    b.breaker.window = Duration::from_secs(3);
                    b.breaker.probe_period = Duration::from_millis(250);
                    b.client.rotation = Duration::from_secs(30);
                    b
                },
            }
        );
    }

    fn url(value: &str) -> Result<Option<String>, String> {
        limiter_url_from_lookup(|key| (key == "LIMITER_URL").then(|| value.to_string()))
    }

    #[test]
    fn a_well_formed_limiter_url_names_its_host_and_port() {
        assert_eq!(url(""), Ok(None));
        assert_eq!(url("  "), Ok(None));
        for (value, hostport) in [
            ("http://limiter:8080", "limiter:8080"),
            ("http://limiter:8080/", "limiter:8080"),
            ("limiter.ns.svc.cluster.local:8080", "limiter.ns.svc.cluster.local:8080"),
            ("http://10.0.0.1:8080", "10.0.0.1:8080"),
            ("http://[::1]:8080", "[::1]:8080"),
            ("HTTP://limiter:8080", "limiter:8080"),
            ("Http://limiter:8080", "limiter:8080"),
        ] {
            assert_eq!(url(value), Ok(Some(hostport.to_string())), "{value}");
        }
    }

    #[test]
    fn a_malformed_limiter_url_refuses_boot() {
        for value in [
            "https://limiter:8080",
            "ftp://limiter:8080",
            "http://limiter",
            "limiter",
            "http://limiter:8080/v1",
            "http://limiter:8080/v1/",
            "http://limiter:8080?x=1",
            "http://user@limiter:8080",
            "http://:8080",
            "http://limiter:0",
            "http://limiter:http",
            "http://limiter:70000",
            "::1:8080",
            "http://::1:8080",
            "http://[::1:8080",
        ] {
            let e = url(value).expect_err(value);
            assert!(e.contains("LIMITER_URL"), "{e}");
        }
    }

    #[test]
    fn the_consecutive_failures_knob_refuses_boot() {
        let e = from(&[("LIMITER_BREAKER_FAILURES", "3")]).expect_err("not a knob");
        assert!(e.contains("LIMITER_BREAKER_REQUESTS"), "{e}");
    }

    #[test]
    fn zero_refuses_boot() {
        for key in KEYS {
            let e = from(&[(key, "0")]).expect_err("zero is refused");
            assert!(e.contains(key), "{e}");
        }
    }

    #[test]
    fn an_unparsable_value_refuses_boot() {
        for key in KEYS {
            let e = from(&[(key, "2s")]).expect_err("an unparsable value is refused");
            assert!(e.contains(key), "{e}");
        }
    }
}
