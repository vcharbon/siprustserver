//! Wall-clock readings as signed epoch milliseconds, and the boot refusal of
//! a wall clock set before the UNIX epoch.

use std::time::{SystemTime, UNIX_EPOCH};

/// The host's wall clock reads before the UNIX epoch: a grossly misconfigured
/// clock. A node anchored there would skew every replicated deadline it
/// restores by decades, so a runner refuses to start on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreEpochClock {
    /// How far before the epoch the clock reads, in milliseconds.
    pub before_ms: i64,
}

impl std::fmt::Display for PreEpochClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the system wall clock reads {} ms before the UNIX epoch (a misconfigured host clock)",
            self.before_ms
        )
    }
}

impl std::error::Error for PreEpochClock {}

pub use call::{millis_i64, millis_u64};

/// `at` as signed milliseconds since the UNIX epoch: negative before it,
/// held at the `i64` bounds.
pub fn epoch_ms(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => millis_i64(after),
        Err(before) => millis_i64(before.duration()).saturating_neg(),
    }
}

/// `at` as epoch milliseconds, refused when it lies before the epoch.
pub fn checked_epoch_ms(at: SystemTime) -> Result<i64, PreEpochClock> {
    match epoch_ms(at) {
        ms if ms < 0 => Err(PreEpochClock { before_ms: ms.saturating_neg() }),
        ms => Ok(ms),
    }
}
