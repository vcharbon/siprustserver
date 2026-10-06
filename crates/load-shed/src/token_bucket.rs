//! A lazy-refill token bucket that owns no clock.

use std::time::Duration;

/// The wait (seconds) a bucket with no refill rate reports: a finite hint for
/// a misconfigured `rate == 0`, which would otherwise never hold a token again.
pub const ZERO_RATE_WAIT_SEC: u32 = 60;

/// A millisecond timestamp as a point on a bucket's timeline, for a caller
/// whose clock reads epoch-ms. A timestamp before the epoch reads as the epoch.
pub fn at_ms(now_ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(now_ms).unwrap_or(0))
}

/// Tokens accrue continuously at `rate_per_sec` up to `capacity`; a take
/// succeeds iff at least one whole token is there. The level stays within
/// `[0, capacity]`: nothing takes past empty, so a drained bucket holds a token
/// again `1 / rate` seconds later, whatever was admitted around it.
///
/// **Clock — the caller's.** Every operation takes `now`, a point on the
/// caller's monotonic timeline expressed as the time since that timeline's
/// origin. The bucket credits time up to the latest `now` it has seen: a `now`
/// earlier than that credits nothing, and returning to an instant already
/// credited mints nothing a second time.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Current level, in `[0, capacity]`.
    tokens: f64,
    capacity: f64,
    rate_per_sec: f64,
    /// The latest instant whose elapsed time is already credited.
    credited_to: Duration,
}

impl TokenBucket {
    /// A full bucket (`level == capacity`) refilling at `rate_per_sec`, with
    /// time credited up to `now`.
    pub fn full(capacity: f64, rate_per_sec: f64, now: Duration) -> Self {
        Self { tokens: capacity, capacity, rate_per_sec, credited_to: now }
    }

    /// Take one token. `Err` leaves the bucket untouched and carries the
    /// seconds until a token, read from the same refill as the failed take, so
    /// it is always ≥ 1 ([`ZERO_RATE_WAIT_SEC`] when the rate is zero).
    pub fn try_take(&mut self, now: Duration) -> Result<(), u32> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        Err(self.wait_after_refill())
    }

    /// Seconds until a token is there: `0` when one is there now.
    pub fn wait_sec(&mut self, now: Duration) -> u32 {
        self.refill(now);
        if self.tokens >= 1.0 {
            return 0;
        }
        self.wait_after_refill()
    }

    /// The most tokens the bucket holds.
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    /// Current level, after a refill.
    pub fn level(&mut self, now: Duration) -> f64 {
        self.refill(now);
        self.tokens
    }

    /// Change capacity and rate at `now`. Time up to `now` is credited at the
    /// old rate first; the level is then clamped to the new capacity, so a
    /// lowered capacity binds at once.
    pub fn set_rate(&mut self, capacity: f64, rate_per_sec: f64, now: Duration) {
        self.refill(now);
        self.capacity = capacity;
        self.rate_per_sec = rate_per_sec;
        self.tokens = self.tokens.min(capacity);
    }

    /// Credit the time between the last credited instant and `now`, capped at
    /// `capacity`. A no-op when `now` is not past the last credited instant.
    fn refill(&mut self, now: Duration) {
        let Some(elapsed) = now.checked_sub(self.credited_to) else { return };
        if elapsed.is_zero() {
            return;
        }
        self.tokens = self.capacity.min(self.tokens + elapsed.as_secs_f64() * self.rate_per_sec);
        self.credited_to = now;
    }

    /// Seconds until the next whole token, for a refilled level below one.
    fn wait_after_refill(&self) -> u32 {
        if self.rate_per_sec <= 0.0 {
            return ZERO_RATE_WAIT_SEC;
        }
        // `tokens < 1` here, so the quotient is positive and its ceiling ≥ 1.
        ((1.0 - self.tokens) / self.rate_per_sec).ceil() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn a_millisecond_timestamp_maps_onto_the_timeline() {
        assert_eq!(at_ms(1500), ms(1500));
        assert_eq!(at_ms(-5), Duration::ZERO, "before the epoch reads as the epoch");
    }

    /// A clock reading before the epoch maps to the epoch, so it credits
    /// nothing past what the epoch already did.
    #[test]
    fn a_reading_before_the_epoch_credits_nothing() {
        let mut b = TokenBucket::full(1.0, 1.0, at_ms(-2000));
        assert_eq!(b.try_take(at_ms(-2000)), Ok(()));
        assert!(b.try_take(at_ms(-1000)).is_err(), "a second before the epoch reads as the epoch");
        assert_eq!(b.try_take(at_ms(1000)), Ok(()));
    }

    #[test]
    fn a_burst_is_absorbed_then_the_bucket_refuses() {
        let mut b = TokenBucket::full(10.0, 1.0, ms(0));
        for i in 0..10 {
            assert_eq!(b.try_take(ms(0)), Ok(()), "burst token {i}");
        }
        assert_eq!(b.try_take(ms(0)), Err(1), "the 11th take in the same instant is refused");
    }

    #[test]
    fn a_take_on_an_empty_bucket_leaves_it_at_zero() {
        let mut b = TokenBucket::full(1.0, 0.0, ms(0));
        assert_eq!(b.level(ms(0)), 1.0);
        assert_eq!(b.try_take(ms(0)), Ok(()));
        assert_eq!(b.try_take(ms(0)), Err(ZERO_RATE_WAIT_SEC));
        assert_eq!(b.level(ms(0)), 0.0);
    }

    #[test]
    fn refill_paces_takes_at_the_rate() {
        let mut b = TokenBucket::full(1.0, 1.0, ms(0));
        assert_eq!(b.try_take(ms(0)), Ok(()));
        assert!(b.try_take(ms(999)).is_err(), "less than a second buys no token");
        assert_eq!(b.try_take(ms(1000)), Ok(()), "one second buys exactly one token");
        assert!(b.try_take(ms(1000)).is_err());
    }

    #[test]
    fn refill_never_exceeds_the_capacity() {
        let mut b = TokenBucket::full(3.0, 10.0, ms(0));
        for _ in 0..3 {
            assert_eq!(b.try_take(ms(0)), Ok(()));
        }
        assert_eq!(b.level(ms(3_600_000)), 3.0, "an hour idle refills to the cap, not beyond");
    }

    /// The hint comes from the refill that failed the take. Judged at 999 µs
    /// (0.999 tokens at 1000/s), the take fails and hints 1 s; a hint read from
    /// a later refill would read 0.
    #[test]
    fn a_failed_take_never_hints_retry_now() {
        let mut b = TokenBucket::full(1.0, 1000.0, Duration::ZERO);
        assert_eq!(b.try_take(Duration::ZERO), Ok(()));
        assert_eq!(b.try_take(Duration::from_micros(999)), Err(1), "0.999 of a token is not one");
        assert_eq!(b.try_take(Duration::from_micros(1001)), Ok(()), "the token accrued after");
    }

    /// The hint rounds up to whole seconds.
    #[test]
    fn the_wait_rounds_up_to_whole_seconds() {
        let mut b = TokenBucket::full(1.0, 0.25, ms(0));
        assert_eq!(b.try_take(ms(0)), Ok(()));
        assert_eq!(b.try_take(ms(0)), Err(4));
        assert_eq!(b.wait_sec(ms(1000)), 3, "0.25 accrued, 0.75 left at 0.25/s");
        assert_eq!(b.wait_sec(ms(3500)), 1, "0.875 accrued: half a second, rounded up");
        assert_eq!(b.wait_sec(ms(4000)), 0, "a token is there");
    }

    #[test]
    fn a_backwards_step_grants_no_tokens() {
        let mut b = TokenBucket::full(1.0, 1.0, ms(10_000));
        assert_eq!(b.try_take(ms(10_000)), Ok(()));
        assert!(b.try_take(ms(0)).is_err(), "time going backwards must not mint a token");
        assert!(b.try_take(ms(0)).is_err());
    }

    #[test]
    fn a_step_back_and_forward_re_mints_nothing_already_credited() {
        let mut b = TokenBucket::full(1.0, 1.0, ms(0));
        assert_eq!(b.try_take(ms(10_000)), Ok(()), "the starting token");
        assert!(b.try_take(ms(0)).is_err(), "a step backwards refills nothing");
        assert!(b.try_take(ms(10_000)).is_err(), "an instant already credited buys nothing");
        assert_eq!(b.try_take(ms(11_000)), Ok(()), "a second of new time buys one");
    }

    /// Time before a rate change is credited at the old rate.
    #[test]
    fn a_rate_change_credits_the_time_before_it_at_the_old_rate() {
        let mut b = TokenBucket::full(100.0, 10.0, ms(0));
        for _ in 0..100 {
            assert_eq!(b.try_take(ms(0)), Ok(()));
        }
        b.set_rate(100.0, 100.0, ms(500));
        assert_eq!(b.level(ms(500)), 5.0, "500 ms at 10/s, not at 100/s");
        assert_eq!(b.level(ms(600)), 15.0, "then 100 ms at 100/s");
    }

    /// A lowered capacity binds at once: the level above it is cut, not spent.
    #[test]
    fn a_lowered_capacity_clamps_the_level_at_once() {
        let mut b = TokenBucket::full(100.0, 100.0, ms(0));
        b.set_rate(2.0, 2.0, ms(0));
        assert_eq!(b.capacity(), 2.0);
        assert_eq!(b.level(ms(0)), 2.0);
        assert_eq!(b.try_take(ms(0)), Ok(()));
        assert_eq!(b.try_take(ms(0)), Ok(()));
        assert_eq!(b.try_take(ms(0)), Err(1));
    }
}
