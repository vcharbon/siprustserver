//! The loop-liveness env grammar.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `B2BUA_LOOP_STALL_MS` | how long one turn of the transaction owner or router may run before the loop reads stalled | 5000 |
//!
//! A stalled loop turns `/ready` and `/healthz` to 503. The default holds
//! under CPU throttling: the `cpu_starve` chaos primitive caps the container
//! at 0.3 CPU in 100 ms periods, so a throttled turn waits at most one period
//! per slice it runs; a turn reads stalled only after holding the loop for
//! 50 such periods, which no single SIP event, timer or sweep needs.
//!
//! Unset or blank takes the default; zero, or a value that is not a count of
//! milliseconds, refuses boot.

use std::time::Duration;

use crate::stated;

/// The default stall threshold.
pub const DEFAULT_LOOP_STALL: Duration = Duration::from_millis(5_000);

/// The stall threshold `lookup` states. An unparsable or zero value is an
/// `Err` naming it.
pub(crate) fn loop_stall_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Duration, String> {
    const KEY: &str = "B2BUA_LOOP_STALL_MS";
    let Some(v) = stated(lookup(KEY)) else {
        return Ok(DEFAULT_LOOP_STALL);
    };
    match v.trim().parse::<u64>() {
        Ok(0) => Err(format!("{KEY}=0: a loop would read stalled on every turn")),
        Ok(ms) => Ok(Duration::from_millis(ms)),
        Err(e) => Err(format!("{KEY}={v:?}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(value: Option<&str>) -> Result<Duration, String> {
        loop_stall_from_lookup(|_| value.map(str::to_string))
    }

    #[test]
    fn unset_or_blank_takes_the_default_and_a_stated_value_is_taken() {
        assert_eq!(from(None), Ok(DEFAULT_LOOP_STALL));
        assert_eq!(from(Some(" ")), Ok(DEFAULT_LOOP_STALL));
        assert_eq!(from(Some("8000")), Ok(Duration::from_millis(8_000)));
    }

    #[test]
    fn zero_or_a_value_that_is_not_milliseconds_refuses_boot() {
        assert!(from(Some("0")).is_err());
        assert!(from(Some("5s")).is_err());
    }
}
