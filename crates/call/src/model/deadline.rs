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
    /// A wait stated in seconds: absent, zero or negative is off.
    pub fn of_secs(secs: Option<i64>) -> Self {
        Self::of_millis(secs.map(|s| s.saturating_mul(1000)))
    }

    /// A wait stated in milliseconds: absent, zero or negative is off.
    pub fn of_millis(millis: Option<i64>) -> Self {
        match millis {
            Some(ms) if ms > 0 => Deadline::After(Duration::from_millis(ms as u64)),
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

    /// The wait in milliseconds, when the deadline is on.
    pub fn millis(self) -> Option<i64> {
        self.duration().map(|d| d.as_millis() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_zero_and_negative_are_off() {
        assert_eq!(Deadline::of_secs(None), Deadline::Off);
        assert_eq!(Deadline::of_secs(Some(0)), Deadline::Off);
        assert_eq!(Deadline::of_secs(Some(-3)), Deadline::Off);
        assert_eq!(Deadline::of_millis(Some(0)), Deadline::Off);
    }

    #[test]
    fn a_positive_wait_keeps_its_milliseconds() {
        assert_eq!(Deadline::of_secs(Some(30)).millis(), Some(30_000));
        assert_eq!(Deadline::of_millis(Some(750)).millis(), Some(750));
    }
}
