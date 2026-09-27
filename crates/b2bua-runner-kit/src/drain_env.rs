//! The planned exit's env grammar (ADR-0031 D2, ADR-0038 decision 9).
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `B2BUA_DRAIN_GRACE_MS` | the most a drain waits for the live calls | 5000 |
//! | `B2BUA_DRAIN_MIN_MS` | the floor a withdrawn worker's caught-up exit waits out | 1000 |
//! | `B2BUA_DRAIN_RELEASE_FLUSH_MS` | the most the exit then waits for its queued limiter releases | 3000 |
//!
//! Unset or blank takes the default; a value that is not a non-negative
//! integer refuses boot. Zero is a bound like any other: no wait.

use std::time::Duration;

use b2bua::drain::DrainBounds;

use crate::stated;

/// The bounds `lookup` states. An unparsable value is an `Err` naming it.
pub(crate) fn drain_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<DrainBounds, String> {
    let millis = |key: &str, default: u64| -> Result<Duration, String> {
        match stated(lookup(key)) {
            None => Ok(Duration::from_millis(default)),
            Some(v) => v
                .trim()
                .parse::<u64>()
                .map(Duration::from_millis)
                .map_err(|e| format!("{key}={v:?}: {e}")),
        }
    };
    Ok(DrainBounds {
        grace: millis("B2BUA_DRAIN_GRACE_MS", 5_000)?,
        floor: millis("B2BUA_DRAIN_MIN_MS", 1_000)?,
        release_flush: millis("B2BUA_DRAIN_RELEASE_FLUSH_MS", 3_000)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<DrainBounds, String> {
        let pairs: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        drain_from_lookup(|key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }

    #[test]
    fn unset_or_blank_takes_the_defaults() {
        let defaults = DrainBounds {
            grace: Duration::from_millis(5_000),
            floor: Duration::from_millis(1_000),
            release_flush: Duration::from_millis(3_000),
        };
        assert_eq!(from(&[]), Ok(defaults));
        assert_eq!(from(&[("B2BUA_DRAIN_RELEASE_FLUSH_MS", "  ")]), Ok(defaults));
    }

    #[test]
    fn stated_values_are_taken_zero_included() {
        let bounds = from(&[
            ("B2BUA_DRAIN_GRACE_MS", "8000"),
            ("B2BUA_DRAIN_MIN_MS", "0"),
            ("B2BUA_DRAIN_RELEASE_FLUSH_MS", "500"),
        ])
        .unwrap();
        assert_eq!(
            bounds,
            DrainBounds {
                grace: Duration::from_millis(8_000),
                floor: Duration::ZERO,
                release_flush: Duration::from_millis(500),
            }
        );
    }

    #[test]
    fn a_value_that_is_not_a_count_of_milliseconds_refuses_boot() {
        for key in ["B2BUA_DRAIN_GRACE_MS", "B2BUA_DRAIN_MIN_MS", "B2BUA_DRAIN_RELEASE_FLUSH_MS"] {
            for value in ["3s", "-1", "1.5", "abc"] {
                let e = from(&[(key, value)]).expect_err(value);
                assert!(e.contains(key), "{e}");
            }
        }
    }
}
