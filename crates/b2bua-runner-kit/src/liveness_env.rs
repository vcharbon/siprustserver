//! The loop-liveness env grammar.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `B2BUA_LOOP_STALL_MS` | how long one turn of the transaction owner or router may run before the loop reads stalled | the larger of 10000 and `B2BUA_CALL_CONTROL_TIMEOUT_MS` + 5000 |
//! | `B2BUA_LOOP_EXIT_MS` | how long one such turn may run before the worker exits non-zero | the larger of 15000 and `B2BUA_LOOP_STALL_MS` + 5000 |
//!
//! A stalled loop turns `/ready` and `/healthz` to 503. The threshold sits
//! above the longest wait a healthy router turn makes: a turn that needs a
//! call's lock waits out the call's own turn, which holds it across its
//! decision round trip, up to `B2BUA_CALL_CONTROL_TIMEOUT_MS`. A threshold
//! not above that wait by [`LOCK_WAIT_MARGIN`] refuses boot.
//!
//! A loop that stays stalled ends the process, its limiter release flush and
//! CDR close bounded as for a loop task that ended (`shutdown`). The exit
//! threshold sits above the stall threshold, so the worker reads not ready
//! before it goes, and above the decision wait; a value not above both refuses
//! boot. Past the livenessProbe's own bound (three misses 10 s apart) the probe
//! ends the stall first.
//!
//! CPU throttling: the `cpu_starve` chaos primitive caps the container's
//! cgroup at 0.3 CPU per 100 ms period, a quota every runnable thread
//! shares, so with N runtime threads busy one turn's thread gets about 0.3/N
//! of a CPU. At 10 s a turn reads stalled only when its own work needs more
//! than 3/N CPU-seconds, 375 ms with 8 busy threads; at the 15 s exit, more
//! than 4.5/N, 562 ms; one SIP event, timer firing or sweep takes
//! milliseconds.
//!
//! Unset or blank takes the default; zero, or a value that is not a count of
//! milliseconds, refuses boot.

use std::time::Duration;

use crate::stated;

/// The default stall threshold when the call-control timeout leaves room.
pub const DEFAULT_LOOP_STALL: Duration = Duration::from_millis(10_000);

/// The default exit threshold when the stall threshold leaves room.
pub const DEFAULT_LOOP_EXIT: Duration = Duration::from_millis(15_000);

/// How far the stall threshold sits above the call-control timeout: the rest
/// of a call turn that holds its lock past the decision round trip.
pub const LOCK_WAIT_MARGIN: Duration = Duration::from_millis(2_000);

/// The margin the default leaves above the call-control timeout.
const DEFAULT_ABOVE_CALL_CONTROL: Duration = Duration::from_millis(5_000);

/// The margin the default exit threshold leaves above the stall threshold.
const DEFAULT_EXIT_ABOVE_STALL: Duration = Duration::from_millis(5_000);

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

/// The exit threshold `lookup` states, the default derived from `loop_stall`.
/// An unparsable or zero value is an `Err` naming it.
pub(crate) fn loop_exit_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
    loop_stall: Duration,
) -> Result<Duration, String> {
    const KEY: &str = "B2BUA_LOOP_EXIT_MS";
    let Some(v) = stated(lookup(KEY)) else {
        return Ok(DEFAULT_LOOP_EXIT.max(loop_stall + DEFAULT_EXIT_ABOVE_STALL));
    };
    match v.trim().parse::<u64>() {
        Ok(0) => Err(format!("{KEY}=0: a worker would exit on every turn")),
        Ok(ms) => Ok(Duration::from_millis(ms)),
        Err(e) => Err(format!("{KEY}={v:?}: {e}")),
    }
}

/// `Err` when `loop_exit` is not above `loop_stall` and the decision wait
/// ([`LOCK_WAIT_MARGIN`] above `call_control_timeout`): a worker would exit
/// reading ready, on a turn a slow decision backend holds, or both.
pub(crate) fn check_loop_exit(
    loop_exit: Duration,
    loop_stall: Duration,
    call_control_timeout: Duration,
) -> Result<(), String> {
    if loop_exit > loop_stall && loop_exit > call_control_timeout + LOCK_WAIT_MARGIN {
        return Ok(());
    }
    Err(format!(
        "B2BUA_LOOP_EXIT_MS={} must exceed B2BUA_LOOP_STALL_MS={} and \
         B2BUA_CALL_CONTROL_TIMEOUT_MS={} by more than {} ms: a worker would exit before it reads \
         not ready, or on a router turn waiting on a call's lock across its decision",
        loop_exit.as_millis(),
        loop_stall.as_millis(),
        call_control_timeout.as_millis(),
        LOCK_WAIT_MARGIN.as_millis()
    ))
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

    fn exit_from(value: Option<&str>, stall: Duration) -> Result<Duration, String> {
        loop_exit_from_lookup(|_| value.map(str::to_string), stall)
    }

    #[test]
    fn the_exit_threshold_defaults_above_the_stall_threshold_and_a_stated_value_is_taken() {
        assert_eq!(exit_from(None, DEFAULT_LOOP_STALL), Ok(DEFAULT_LOOP_EXIT));
        assert_eq!(exit_from(Some(" "), DEFAULT_LOOP_STALL), Ok(DEFAULT_LOOP_EXIT));
        let long = Duration::from_millis(14_000);
        assert_eq!(exit_from(None, long), Ok(Duration::from_millis(19_000)));
        assert_eq!(exit_from(Some("20000"), long), Ok(Duration::from_millis(20_000)));
        assert!(exit_from(Some("0"), long).is_err());
        assert!(exit_from(Some("15s"), long).is_err());
    }

    #[test]
    fn an_exit_threshold_not_above_the_stall_threshold_or_the_decision_wait_refuses_boot() {
        let stall = DEFAULT_LOOP_STALL;
        assert_eq!(check_loop_exit(DEFAULT_LOOP_EXIT, stall, CALL_CONTROL), Ok(()));
        let refused = check_loop_exit(stall, stall, CALL_CONTROL).unwrap_err();
        for variable in
            ["B2BUA_LOOP_EXIT_MS", "B2BUA_LOOP_STALL_MS", "B2BUA_CALL_CONTROL_TIMEOUT_MS"]
        {
            assert!(refused.contains(variable), "{refused}");
        }
        let long_call_control = DEFAULT_LOOP_EXIT;
        assert!(check_loop_exit(DEFAULT_LOOP_EXIT, stall, long_call_control).is_err());
    }
}
