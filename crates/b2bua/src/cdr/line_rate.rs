//! The rate limit of a CDR sink's repeated log line (a nack, a panic caught
//! per record): a storm of the events it reports costs one line per
//! [`LINE_INTERVAL`], carrying the events since the line before, whatever
//! their rate. A counter counts each event; the line is the evidence.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sip_clock::wall::millis_u64;
use tokio::time::Instant;

/// The shortest time between two lines of one [`LineRate`].
pub const LINE_INTERVAL: Duration = Duration::from_secs(60);

/// The events one line reports, and when its next line is due.
#[derive(Debug)]
pub struct LineRate {
    origin: Instant,
    /// Events no line carried yet.
    unlogged: AtomicU64,
    /// When the next line is due, in milliseconds since `origin`.
    next_line_ms: AtomicU64,
}

impl Default for LineRate {
    fn default() -> Self {
        Self::new()
    }
}

impl LineRate {
    /// A rate whose first event is due a line at once.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            unlogged: AtomicU64::new(0),
            next_line_ms: AtomicU64::new(0),
        }
    }

    /// Counts one event. `Some(n)` when a line is due now, `n` the events
    /// since the last line, this one included; the next line is then due
    /// [`LINE_INTERVAL`] later. One caller alone gets each line.
    pub fn counted(&self) -> Option<u64> {
        self.unlogged.fetch_add(1, Ordering::Relaxed);
        let now = millis_u64(self.origin.elapsed());
        let due = self.next_line_ms.load(Ordering::Relaxed);
        if now < due {
            return None;
        }
        let next = now.saturating_add(millis_u64(LINE_INTERVAL));
        self.next_line_ms.compare_exchange(due, next, Ordering::Relaxed, Ordering::Relaxed).ok()?;
        Some(self.unlogged.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// A storm of events is one line per interval: the first event's at once,
    /// then one at the first event past each interval, carrying every event
    /// since the line before.
    #[tokio::test(start_paused = true)]
    async fn a_storm_logs_once_per_interval_with_the_count_since_the_last_line() {
        let rate = LineRate::new();
        assert_eq!(rate.counted(), Some(1), "the first event is said at once");
        let storm: Vec<_> = (0..999).filter_map(|_| rate.counted()).collect();
        assert_eq!(storm, Vec::<u64>::new(), "no line inside the interval");
        tokio::time::advance(LINE_INTERVAL - MS).await;
        assert_eq!(rate.counted(), None, "the interval has not run out");
        tokio::time::advance(MS).await;
        assert_eq!(rate.counted(), Some(1_001), "the storm and the two events after it");
        assert_eq!(rate.counted(), None);
        tokio::time::advance(3 * LINE_INTERVAL).await;
        assert_eq!(rate.counted(), Some(2), "a quiet spell carries the events left over");
    }
}
