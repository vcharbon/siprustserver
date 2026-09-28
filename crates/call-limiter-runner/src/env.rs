//! The limiter process's env grammar.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `LIMITER_LISTEN` | the `ip:port` the API, `/metrics` and `/healthz` bind | `0.0.0.0:8080` |
//! | `LIMITER_LEASE_SECONDS` | the lease every admit and refresh answer states; the workers refresh below it | 120 |
//! | `LIMITER_JANITOR_INTERVAL_SECONDS` | how often an idle store drops the sets whose lease lapsed | 10 |
//!
//! Unset or blank takes the default. A `LIMITER_LISTEN` that is not a socket
//! address, or a seconds value that is not a whole number in `1..=86400`
//! (one day), refuses boot.

use std::net::SocketAddr;

use call_limiter::{DEFAULT_LEASE_SEC, MAX_LEASE_SEC};

/// The janitor period unless configured otherwise (seconds).
const DEFAULT_JANITOR_SEC: i64 = 10;

/// The limiter process's settings, as the env states them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunnerEnv {
    pub listen: SocketAddr,
    pub lease_sec: i64,
    pub janitor_sec: u64,
}

/// The settings `lookup` states; an `Err` names the first variable it refuses.
pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<RunnerEnv, String> {
    let stated = |key: &str| lookup(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let listen = match stated("LIMITER_LISTEN") {
        None => SocketAddr::from(([0, 0, 0, 0], 8080)),
        Some(v) => v.parse().map_err(|e| format!("LIMITER_LISTEN={v:?}: {e}"))?,
    };
    let seconds = |key: &str, default: i64| -> Result<i64, String> {
        let Some(v) = stated(key) else {
            return Ok(default);
        };
        match v.parse::<i64>() {
            Ok(n) if (1..=MAX_LEASE_SEC).contains(&n) => Ok(n),
            Ok(n) => Err(format!("{key}={n}: not in 1..={MAX_LEASE_SEC}")),
            Err(e) => Err(format!("{key}={v:?}: {e}")),
        }
    };
    Ok(RunnerEnv {
        listen,
        lease_sec: seconds("LIMITER_LEASE_SECONDS", DEFAULT_LEASE_SEC)?,
        janitor_sec: stated("LIMITER_JANITOR_INTERVAL_SECONDS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_JANITOR_SEC as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<RunnerEnv, String> {
        from_lookup(|key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
    }

    const DEFAULTS: RunnerEnv = RunnerEnv {
        listen: SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8080),
        lease_sec: DEFAULT_LEASE_SEC,
        janitor_sec: DEFAULT_JANITOR_SEC as u64,
    };

    #[test]
    fn unset_or_blank_takes_the_defaults() {
        assert_eq!(from(&[]), Ok(DEFAULTS));
        assert_eq!(
            from(&[
                ("LIMITER_LISTEN", " "),
                ("LIMITER_LEASE_SECONDS", ""),
                ("LIMITER_JANITOR_INTERVAL_SECONDS", "  "),
            ]),
            Ok(DEFAULTS)
        );
    }

    #[test]
    fn stated_values_are_taken() {
        assert_eq!(
            from(&[
                ("LIMITER_LISTEN", "127.0.0.1:9000"),
                ("LIMITER_LEASE_SECONDS", "60"),
                ("LIMITER_JANITOR_INTERVAL_SECONDS", " 5 "),
            ]),
            Ok(RunnerEnv {
                listen: "127.0.0.1:9000".parse().unwrap(),
                lease_sec: 60,
                janitor_sec: 5,
            })
        );
        let max = MAX_LEASE_SEC.to_string();
        let env =
            from(&[("LIMITER_LEASE_SECONDS", &max), ("LIMITER_JANITOR_INTERVAL_SECONDS", &max)])
                .unwrap();
        assert_eq!((env.lease_sec, env.janitor_sec), (MAX_LEASE_SEC, MAX_LEASE_SEC as u64));
    }

    #[test]
    fn a_seconds_value_outside_one_second_to_one_day_refuses_boot() {
        for key in ["LIMITER_LEASE_SECONDS", "LIMITER_JANITOR_INTERVAL_SECONDS"] {
            for bad in ["0", "-5", "2m", "1.5", "86401", "18446744073709551616"] {
                let e = from(&[(key, bad)]).expect_err(&format!("{key}={bad}"));
                assert!(e.contains(key), "{e}");
            }
        }
    }

    #[test]
    fn a_listen_value_that_is_not_a_socket_address_refuses_boot() {
        for bad in ["0.0.0.0", "limiter:8080", ":8080", "0.0.0.0:99999"] {
            let e = from(&[("LIMITER_LISTEN", bad)]).expect_err(bad);
            assert!(e.contains("LIMITER_LISTEN"), "{e}");
        }
    }
}
