//! The dead-verdict bounds a runner reads from env at boot (ADR-0050 §3,
//! ADR-0047 D1).
//!
//! ```text
//!   B2BUA_DEAD_NOTICE_FRESH_MS    how far a proxy's dead verdict's instant may
//!                                 stand from this worker's clock, either way;
//!                                 a notice past it is refused as stale  (10000)
//!   B2BUA_DEAD_VERDICT_HOLD_MS    how long a taken verdict is held: a repeat
//!                                 of its notice inside it is the same
//!                                 verdict                               (42000)
//! ```
//!
//! Unset takes the default. A value that is not a positive integer refuses the
//! boot naming its variable, and so does a hold under the freshness window
//! plus Timer F (32 s): the proxy repeats a notice on Timer E up to Timer F,
//! and a repeat the hold no longer covers would be taken as a new verdict
//! once and refused as stale after. The freshness window spans the proxies'
//! and the workers' clocks, which it must cover; a wider one accepts a replay
//! longer.

use b2bua::repl::VerdictBounds;

use super::link_env::positive;

/// Reads the verdict bounds through `get`; `Err` names the variables.
pub(crate) fn verdict_bounds(
    get: impl Fn(&str) -> Option<String>,
) -> Result<VerdictBounds, String> {
    let d = VerdictBounds::default();
    let ms = |key: &str, default: i64| {
        let v = positive(&get, key, u64::try_from(default).unwrap_or(0))?;
        i64::try_from(v).map_err(|_| format!("{key} is out of range"))
    };
    let bounds = VerdictBounds {
        fresh_ms: ms("B2BUA_DEAD_NOTICE_FRESH_MS", d.fresh_ms)?,
        hold_ms: ms("B2BUA_DEAD_VERDICT_HOLD_MS", d.hold_ms)?,
    };
    bounds
        .validate()
        .map_err(|why| format!("B2BUA_DEAD_VERDICT_HOLD_MS / B2BUA_DEAD_NOTICE_FRESH_MS: {why}"))?;
    Ok(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<VerdictBounds, String> {
        verdict_bounds(|key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn unset_takes_the_defaults() {
        assert_eq!(read(&[]), Ok(VerdictBounds::default()));
    }

    #[test]
    fn each_variable_is_read() {
        let b = read(&[
            ("B2BUA_DEAD_NOTICE_FRESH_MS", "5000"),
            ("B2BUA_DEAD_VERDICT_HOLD_MS", "40000"),
        ])
        .unwrap();
        assert_eq!((b.fresh_ms, b.hold_ms), (5_000, 40_000));
    }

    #[test]
    fn a_hold_under_the_window_plus_timer_f_refuses_the_boot_naming_both() {
        let err = read(&[("B2BUA_DEAD_NOTICE_FRESH_MS", "20000")]).unwrap_err();
        assert!(
            err.contains("B2BUA_DEAD_VERDICT_HOLD_MS")
                && err.contains("B2BUA_DEAD_NOTICE_FRESH_MS"),
            "{err}"
        );
    }

    #[test]
    fn a_bound_that_is_not_a_positive_integer_refuses_the_boot() {
        for key in ["B2BUA_DEAD_NOTICE_FRESH_MS", "B2BUA_DEAD_VERDICT_HOLD_MS"] {
            assert!(read(&[(key, "0")]).unwrap_err().contains(key));
        }
    }
}
