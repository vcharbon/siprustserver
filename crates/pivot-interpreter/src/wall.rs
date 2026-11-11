//! The **wall clock** a run's timing states beside its own clock. It reads the
//! system time, never `tokio::time`, which a paused runtime holds still.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix milliseconds now; zero on a clock set before the epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
