//! The call limiter client's env grammar (ADR-0038).
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `LIMITER_TIMEOUT_MS` | the budget of one admit or refresh (fail-open past it) | 150 |
//! | `LIMITER_REFRESH_SECONDS` | how often a counted call extends its lease | 40 |
//! | `LIMITER_LEASE_SECONDS` | the limiter's lease: a release queued longer is given up | 120 |
//! | `LIMITER_RELEASE_TIMEOUT_MS` | the budget of one release request | 2000 |
//! | `LIMITER_RELEASE_QUEUE_CAP` | most releases the queue holds | 100000 |
//!
//! Unset or blank takes the default; a value that is not a positive integer
//! refuses boot.

use crate::stated;

/// The limiter client's settings, as the env states them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimiterEnv {
    pub timeout_ms: u64,
    pub refresh_sec: i64,
    pub lease_sec: i64,
    pub release_timeout_ms: u64,
    pub queue_cap: usize,
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
    Ok(LimiterEnv {
        timeout_ms: positive("LIMITER_TIMEOUT_MS", 150)?,
        refresh_sec: seconds("LIMITER_REFRESH_SECONDS", 40)?,
        lease_sec: seconds("LIMITER_LEASE_SECONDS", 120)?,
        release_timeout_ms: positive("LIMITER_RELEASE_TIMEOUT_MS", 2_000)?,
        queue_cap: positive("LIMITER_RELEASE_QUEUE_CAP", 100_000)? as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 5] = [
        "LIMITER_TIMEOUT_MS",
        "LIMITER_REFRESH_SECONDS",
        "LIMITER_LEASE_SECONDS",
        "LIMITER_RELEASE_TIMEOUT_MS",
        "LIMITER_RELEASE_QUEUE_CAP",
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
                refresh_sec: 40,
                lease_sec: 120,
                release_timeout_ms: 2_000,
                queue_cap: 100_000
            }
        );
    }

    #[test]
    fn stated_values_are_taken() {
        let env = from(&[
            ("LIMITER_TIMEOUT_MS", "100"),
            ("LIMITER_REFRESH_SECONDS", "20"),
            ("LIMITER_LEASE_SECONDS", "60"),
            ("LIMITER_RELEASE_TIMEOUT_MS", "500"),
            ("LIMITER_RELEASE_QUEUE_CAP", "10"),
        ])
        .unwrap();
        assert_eq!(
            env,
            LimiterEnv {
                timeout_ms: 100,
                refresh_sec: 20,
                lease_sec: 60,
                release_timeout_ms: 500,
                queue_cap: 10
            }
        );
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
