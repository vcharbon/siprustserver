//! A per-call timer's delay, carrying its unit ([`TimerDelay`]), and the
//! longest delay the engine arms ([`MAX_DELAY_MS`]).

use call::features::PlatformActivations;

/// The longest delay the engine arms a timer for, in milliseconds: `i32::MAX`,
/// about 24.8 days. It sits above any wait a call needs, and a deadline it
/// puts after any realistic clock stays far from `i64::MAX`. The executor
/// arms a longer delay at this maximum and notes the defect on the turn.
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

/// Hold the call's maximum duration in `platform` to [`MAX_DELAY_MS`]: the
/// `GlobalDuration` deadline it arms and the per-call caps that grow with it
/// read the held value.
pub fn hold_max_duration(platform: &mut PlatformActivations) {
    platform.max_duration_ms = platform.max_duration_ms.min(MAX_DELAY_MS);
}

#[cfg(test)]
mod tests {
    use call::features::KeepaliveActivation;

    use super::*;

    fn platform(max_duration_ms: i64) -> PlatformActivations {
        PlatformActivations {
            max_duration_ms,
            max_duration_anchor: Default::default(),
            keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
        }
    }

    #[test]
    fn seconds_past_i64_milliseconds_read_as_the_longest_delay() {
        assert_eq!(TimerDelay::secs(i64::MAX).as_millis(), i64::MAX);
        assert_eq!(TimerDelay::secs(i64::MIN).as_millis(), i64::MIN);
        assert_eq!(TimerDelay::secs(30).as_millis(), 30_000);
    }

    #[test]
    fn a_max_duration_past_the_maximum_delay_is_held_to_it() {
        let mut held = platform(i64::MAX);
        hold_max_duration(&mut held);
        assert_eq!(held.max_duration_ms, 2_147_483_647);
    }

    #[test]
    fn a_max_duration_within_the_maximum_delay_keeps_its_value() {
        let mut held = platform(3_600_000);
        hold_max_duration(&mut held);
        assert_eq!(held.max_duration_ms, 3_600_000);
    }
}
