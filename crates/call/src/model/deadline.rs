//! [`Deadline`]: a configured wait that is either off or a duration.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A wait a routing answer or the configuration states: off, or a duration
/// honoured at millisecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deadline {
    /// No deadline: nothing is armed.
    #[default]
    Off,
    /// The wait, never zero.
    After(Duration),
}

impl Deadline {
    /// A wait stated in milliseconds: absent, zero or negative is off.
    pub fn of_millis(millis: Option<i64>) -> Self {
        match millis.and_then(|ms| u64::try_from(ms).ok()) {
            Some(ms) if ms > 0 => Deadline::After(Duration::from_millis(ms)),
            _ => Deadline::Off,
        }
    }

    /// The wait, when the deadline is on.
    pub fn duration(self) -> Option<Duration> {
        match self {
            Deadline::Off => None,
            Deadline::After(d) => Some(d),
        }
    }

    /// The wait in milliseconds, when the deadline is on; a wait past
    /// `i64::MAX` milliseconds reads as `i64::MAX`.
    pub fn millis(self) -> Option<i64> {
        self.duration().map(millis_i64)
    }
}

/// `d` in whole milliseconds, held at `i64::MAX`.
pub fn millis_i64(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// `d` in whole milliseconds, held at `u64::MAX`.
pub fn millis_u64(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_zero_and_negative_are_off() {
        assert_eq!(Deadline::of_millis(None), Deadline::Off);
        assert_eq!(Deadline::of_millis(Some(0)), Deadline::Off);
        assert_eq!(Deadline::of_millis(Some(-3)), Deadline::Off);
    }

    #[test]
    fn a_positive_wait_keeps_its_milliseconds() {
        assert_eq!(Deadline::of_millis(Some(750)).millis(), Some(750));
    }

    /// A decoded body may carry any duration: one past `i64` milliseconds
    /// reads as the longest wait, never as a negative one.
    #[test]
    fn a_wait_past_i64_milliseconds_reads_as_the_longest() {
        assert_eq!(Deadline::After(Duration::MAX).millis(), Some(i64::MAX));
        let past = Duration::from_millis(u64::try_from(i64::MAX).unwrap() + 1);
        assert_eq!(Deadline::After(past).millis(), Some(i64::MAX));
    }
}
