//! The reconnect backoff of the RabbitMQ CDR sink: after a failed connection
//! the next attempt waits `min`, doubled per consecutive failure up to `max`,
//! so a dead broker costs one connect attempt per wait, not one per record.

use std::time::Duration;

use tokio::time::Instant;

#[derive(Debug)]
pub(super) struct Backoff {
    min: Duration,
    max: Duration,
    failures: u32,
    retry_at: Option<Instant>,
}

impl Backoff {
    pub(super) fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, failures: 0, retry_at: None }
    }

    /// Whether a connect may be attempted at `now`.
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|t| now >= t)
    }

    /// One more consecutive failure at `now`; returns the wait it sets.
    pub(super) fn failed(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let doublings = (self.failures - 1).min(31);
        let wait = self.min.saturating_mul(1 << doublings).min(self.max);
        self.retry_at = Some(now + wait);
        wait
    }

    /// A connection delivered: the next failure starts from `min` again, and a
    /// connect may be attempted at once.
    pub(super) fn recovered(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_fresh_backoff_allows_a_connect_at_once() {
        assert!(Backoff::new(500 * MS, 5_000 * MS).ready(Instant::now()));
    }

    #[test]
    fn consecutive_failures_double_the_wait_up_to_the_ceiling() {
        let mut b = Backoff::new(500 * MS, 5_000 * MS);
        let t0 = Instant::now();
        let waits: Vec<_> = (0..7).map(|_| b.failed(t0).as_millis()).collect();
        assert_eq!(waits, [500, 1_000, 2_000, 4_000, 5_000, 5_000, 5_000]);
    }

    #[test]
    fn no_connect_is_attempted_before_the_wait_ends() {
        let mut b = Backoff::new(500 * MS, 5_000 * MS);
        let t0 = Instant::now();
        b.failed(t0);
        assert!(!b.ready(t0));
        assert!(!b.ready(t0 + 499 * MS));
        assert!(b.ready(t0 + 500 * MS));
    }

    #[test]
    fn a_recovery_restarts_from_the_floor_and_allows_a_connect_at_once() {
        let mut b = Backoff::new(500 * MS, 5_000 * MS);
        let t0 = Instant::now();
        for _ in 0..4 {
            b.failed(t0);
        }
        b.recovered();
        assert!(b.ready(t0));
        assert_eq!(b.failed(t0), 500 * MS);
    }

    #[test]
    fn the_wait_stays_at_the_ceiling_however_many_failures() {
        let hour = Duration::from_secs(3_600);
        let mut b = Backoff::new(hour, hour);
        let t0 = Instant::now();
        for _ in 0..100 {
            assert_eq!(b.failed(t0), hour);
        }
    }
}
