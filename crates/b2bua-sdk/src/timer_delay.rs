//! A per-call timer's delay, carrying its unit ([`TimerDelay`]), and the
//! longest delay the engine honours for a rule's timer or a route's duration
//! ([`MAX_DELAY_MS`]).

/// The longest delay a rule arms a per-call timer for, and the longest
/// maximum duration a route grants a call, in milliseconds: `i32::MAX`, about
/// 24.8 days. It sits above any wait a call needs, and a deadline it puts
/// after any realistic clock stays far from `i64::MAX`. The executor arms a
/// longer delay at this maximum and notes the defect on the turn; the route
/// fold holds a longer duration to it. The shell's ceiling on an `Instant`
/// deadline, `sip_clock::deadline::MAX_DELAY` (a year), lies above it.
pub const MAX_DELAY_MS: i64 = i32::MAX as i64;

/// A timer delay, carrying its own unit. Every arming site states the unit it
/// means ([`TimerDelay::secs`] / [`TimerDelay::millis`]) and the conversion to
/// the millisecond deadline the engine schedules on happens once, here — a bare
/// integer can never reach [`RuleAction::ScheduleTimer`](crate::model::RuleAction::ScheduleTimer) under the wrong scale.
/// A delay keeps what it states; the engine holds it to [`MAX_DELAY_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimerDelay(i64);

impl TimerDelay {
    /// A delay in whole seconds, its milliseconds held at the bounds of `i64`.
    pub const fn secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1000))
    }

    /// A delay in milliseconds — the granularity the timer driver actually runs
    /// at, for a deadline no whole second can express.
    pub const fn millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The delay in milliseconds (the engine's own unit).
    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_past_i64_milliseconds_read_as_the_longest_delay() {
        assert_eq!(TimerDelay::secs(i64::MAX).as_millis(), i64::MAX);
        assert_eq!(TimerDelay::secs(i64::MIN).as_millis(), i64::MIN);
        assert_eq!(TimerDelay::secs(30).as_millis(), 30_000);
    }
}
