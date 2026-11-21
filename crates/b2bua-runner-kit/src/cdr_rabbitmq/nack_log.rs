//! The rate limit of the RabbitMQ CDR sink's nack warning: a broker that
//! nacks every publish costs one log line per [`NACK_LOG_INTERVAL`], carrying
//! the nacks since the line before, whatever the record rate and however many
//! sessions it spans. `b2bua_cdr_dropped_total{reason="nacked"}` counts each.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sip_clock::wall::millis_u64;
use tokio::time::Instant;

/// The shortest time between two nack lines.
pub(super) const NACK_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// The nacks of one writer, and when its next nack line is due.
#[derive(Debug)]
pub(super) struct NackLog {
    origin: Instant,
    /// Nacks no line carried yet.
    unlogged: AtomicU64,
    /// When the next line is due, in milliseconds since `origin`.
    next_line_ms: AtomicU64,
}

impl NackLog {
    /// A log whose first nack is due a line at once.
    pub(super) fn new() -> Self {
        Self {
            origin: Instant::now(),
            unlogged: AtomicU64::new(0),
            next_line_ms: AtomicU64::new(0),
        }
    }

    /// Counts one nack. `Some(n)` when a line is due now, `n` the nacks since
    /// the last line, this one included; the next line is then due
    /// [`NACK_LOG_INTERVAL`] later. One caller alone gets each line.
    pub(super) fn nacked(&self) -> Option<u64> {
        self.unlogged.fetch_add(1, Ordering::Relaxed);
        let now = millis_u64(self.origin.elapsed());
        let due = self.next_line_ms.load(Ordering::Relaxed);
        if now < due {
            return None;
        }
        let next = now.saturating_add(millis_u64(NACK_LOG_INTERVAL));
        self.next_line_ms.compare_exchange(due, next, Ordering::Relaxed, Ordering::Relaxed).ok()?;
        Some(self.unlogged.swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// A storm of nacks is one line per interval: the first nack's at once,
    /// then one at the first nack past each interval, carrying every nack
    /// since the line before.
    #[tokio::test(start_paused = true)]
    async fn a_nack_storm_logs_once_per_interval_with_the_count_since_the_last_line() {
        let log = NackLog::new();
        assert_eq!(log.nacked(), Some(1), "the first nack is said at once");
        let storm: Vec<_> = (0..999).filter_map(|_| log.nacked()).collect();
        assert_eq!(storm, Vec::<u64>::new(), "no line inside the interval");
        tokio::time::advance(NACK_LOG_INTERVAL - MS).await;
        assert_eq!(log.nacked(), None, "the interval has not run out");
        tokio::time::advance(MS).await;
        assert_eq!(log.nacked(), Some(1_001), "the storm and the two nacks after it");
        assert_eq!(log.nacked(), None);
        tokio::time::advance(3 * NACK_LOG_INTERVAL).await;
        assert_eq!(log.nacked(), Some(2), "a quiet spell carries the nacks left over");
    }
}
