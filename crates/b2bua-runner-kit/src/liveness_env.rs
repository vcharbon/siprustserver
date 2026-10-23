//! The loop-liveness env grammar.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `B2BUA_LOOP_STALL_MS` | how long one turn of the transaction owner or router may run before the loop reads stalled | the larger of 10000 and `B2BUA_CALL_CONTROL_TIMEOUT_MS` + 5000 |
//!
//! A stalled loop turns `/ready` and `/healthz` to 503. The threshold sits
//! above the longest wait a healthy router turn makes: a turn that needs a
//! call's lock waits out the call's own turn, which holds it across its
//! decision round trip, up to `B2BUA_CALL_CONTROL_TIMEOUT_MS`. A threshold
//! not above that wait by [`LOCK_WAIT_MARGIN`] refuses boot.
//!
//! CPU throttling: the `cpu_starve` chaos primitive caps the container's
//! cgroup at 0.3 CPU per 100 ms period, a quota every runnable thread
//! shares, so with N runtime threads busy one turn's thread gets about 0.3/N
//! of a CPU. At 10 s a turn reads stalled only when its own work needs more
//! than 3/N CPU-seconds, 375 ms with 8 busy threads; one SIP event, timer
//! firing or sweep takes milliseconds.
//!
//! Unset or blank takes the default; zero, or a value that is not a count of
//! milliseconds, refuses boot.

use std::time::Duration;

use crate::stated;

/// The default stall threshold when the call-control timeout leaves room.
pub const DEFAULT_LOOP_STALL: Duration = Duration::from_millis(10_000);

/// How far the stall threshold sits above the call-control timeout: the rest
/// of a call turn that holds its lock past the decision round trip.
pub const LOCK_WAIT_MARGIN: Duration = Duration::from_millis(2_000);

/// The margin the default leaves above the call-control timeout.
const DEFAULT_ABOVE_CALL_CONTROL: Duration = Duration::from_millis(5_000);

/// The stall threshold `lookup` states, the default derived from
/// `call_control_timeout`. An unparsable or zero value is an `Err` naming it.
pub(crate) fn loop_stall_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    call_control_timeout: Duration,
) -> Result<Duration, String> {
    const KEY: &str = "B2BUA_LOOP_STALL_MS";
    let Some(v) = stated(lookup(KEY)) else {
        return Ok(DEFAULT_LOOP_STALL.max(call_control_timeout + DEFAULT_ABOVE_CALL_CONTROL));
    };
    match v.trim().parse::<u64>() {
        Ok(0) => Err(format!("{KEY}=0: a loop would read stalled on every turn")),
        Ok(ms) => Ok(Duration::from_millis(ms)),
        Err(e) => Err(format!("{KEY}={v:?}: {e}")),
    }
}

/// `Err` when `loop_stall` does not exceed `call_control_timeout` by
/// [`LOCK_WAIT_MARGIN`]: a slow decision backend would read as a stalled
/// router.
pub(crate) fn check_loop_stall(
    loop_stall: Duration,
    call_control_timeout: Duration,
) -> Result<(), String> {
    let floor = call_control_timeout + LOCK_WAIT_MARGIN;
    if loop_stall > floor {
        return Ok(());
    }
    Err(format!(
        "B2BUA_LOOP_STALL_MS={} must exceed B2BUA_CALL_CONTROL_TIMEOUT_MS={} by more than {} ms: \
         a router turn waiting on a call's lock across its decision would read stalled",
        loop_stall.as_millis(),
        call_control_timeout.as_millis(),
        LOCK_WAIT_MARGIN.as_millis()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL_CONTROL: Duration = Duration::from_millis(5_000);

    fn from(value: Option<&str>, call_control: Duration) -> Result<Duration, String> {
        loop_stall_from_lookup(|_| value.map(str::to_string), call_control)
    }

    #[test]
    fn unset_or_blank_takes_the_default_and_a_stated_value_is_taken() {
        assert_eq!(from(None, CALL_CONTROL), Ok(DEFAULT_LOOP_STALL));
        assert_eq!(from(Some(" "), CALL_CONTROL), Ok(DEFAULT_LOOP_STALL));
        assert_eq!(from(Some("8000"), CALL_CONTROL), Ok(Duration::from_millis(8_000)));
    }

    #[test]
    fn the_default_rises_above_a_long_call_control_timeout() {
        let long = Duration::from_millis(9_000);
        assert_eq!(from(None, long), Ok(Duration::from_millis(14_000)));
        assert_eq!(check_loop_stall(from(None, long).unwrap(), long), Ok(()));
    }

    #[test]
    fn zero_or_a_value_that_is_not_milliseconds_refuses_boot() {
        assert!(from(Some("0"), CALL_CONTROL).is_err());
        assert!(from(Some("5s"), CALL_CONTROL).is_err());
    }

    #[test]
    fn a_threshold_within_the_decision_wait_refuses_boot() {
        assert_eq!(check_loop_stall(DEFAULT_LOOP_STALL, CALL_CONTROL), Ok(()));
        let at_floor = CALL_CONTROL + LOCK_WAIT_MARGIN;
        assert!(check_loop_stall(at_floor, CALL_CONTROL).is_err(), "not above the floor");
        assert!(check_loop_stall(Duration::from_millis(5_000), CALL_CONTROL).is_err());
    }
}
