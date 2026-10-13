//! [`LoopPulse`] — whether a loop that waits for work is turning.
//!
//! A loop idle on an empty socket and a loop parked on one piece of work both
//! show no activity; only the second is stuck. The loop stamps the instant it
//! takes work and clears the stamp when it is back to waiting, so an idle loop
//! is never stalled however long it waits, and a stamp older than a threshold
//! is a loop that is neither waiting nor turning.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

/// One loop's pulse. Shared (`Arc`) between the loop that stamps it and the
/// readers that judge it; lock-free.
#[derive(Debug)]
pub struct LoopPulse {
    /// The origin of the stamps, on `tokio::time`.
    epoch: Instant,
    /// `0` while the loop waits for work, else one plus the milliseconds from
    /// `epoch` to the instant it took the work it is still on.
    busy_since: AtomicU64,
}

impl Default for LoopPulse {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopPulse {
    /// A pulse whose loop is waiting.
    pub fn new() -> Self {
        Self { epoch: Instant::now(), busy_since: AtomicU64::new(0) }
    }

    /// The loop took a piece of work, now.
    pub fn turning(&self) {
        let since = self.epoch.elapsed().as_millis() as u64;
        self.busy_since.store(since + 1, Ordering::Relaxed);
    }

    /// The loop is back to waiting for work.
    pub fn waiting(&self) {
        self.busy_since.store(0, Ordering::Relaxed);
    }

    /// How long the loop has been on its current piece of work; `None` while
    /// it waits.
    pub fn busy_for(&self) -> Option<Duration> {
        match self.busy_since.load(Ordering::Relaxed) {
            0 => None,
            stamp => {
                let now = self.epoch.elapsed().as_millis() as u64;
                Some(Duration::from_millis(now.saturating_sub(stamp - 1)))
            }
        }
    }

    /// Whether the loop has been on one piece of work for longer than
    /// `threshold`.
    pub fn stalled(&self, threshold: Duration) -> bool {
        self.busy_for().is_some_and(|busy| busy > threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    #[tokio::test(start_paused = true)]
    async fn a_waiting_loop_is_never_stalled_however_long_it_waits() {
        let pulse = LoopPulse::new();
        tokio::time::advance(SEC * 3600).await;
        assert_eq!(pulse.busy_for(), None);
        pulse.turning();
        pulse.waiting();
        tokio::time::advance(SEC * 3600).await;
        assert!(!pulse.stalled(SEC));
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_on_one_piece_of_work_past_the_threshold_is_stalled() {
        let pulse = LoopPulse::new();
        pulse.turning();
        tokio::time::advance(SEC).await;
        assert!(!pulse.stalled(SEC), "at the threshold, not past it");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(pulse.stalled(SEC));
        pulse.turning();
        assert!(!pulse.stalled(SEC), "fresh work restarts the clock");
    }

    #[tokio::test(start_paused = true)]
    async fn work_taken_at_the_epoch_still_counts_as_busy() {
        let pulse = LoopPulse::new();
        pulse.turning();
        assert_eq!(pulse.busy_for(), Some(Duration::ZERO));
    }
}
