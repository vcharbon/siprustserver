//! A per-call timer's delay, carrying its unit ([`TimerDelay`]).

/// A timer delay, carrying its own unit. Every arming site states the unit it
/// means ([`TimerDelay::secs`] / [`TimerDelay::millis`]) and the conversion to
/// the millisecond deadline the engine schedules on happens once, here — a bare
/// integer can never reach [`RuleAction::ScheduleTimer`](crate::model::RuleAction::ScheduleTimer) under the wrong scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimerDelay(i64);

impl TimerDelay {
    /// A delay in whole seconds.
    pub const fn secs(secs: i64) -> Self {
        Self(secs * 1000)
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
