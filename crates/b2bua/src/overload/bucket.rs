//! Hard CPS gate: a lazy-refill token bucket riding `tokio::time::Instant`.

/// Lazy-refill token bucket. Tokens accrue continuously at `rate_per_sec` up to
/// `capacity`; [`try_consume`](TokenBucket::try_consume) succeeds iff at least
/// one token is available. The level stays within `[0, capacity]`: nothing
/// consumes past empty, so a drained bucket holds a token again `1 / rate`
/// seconds later, whatever was admitted around it.
///
/// **Clock — rides `tokio::time::Instant`, not wall time.** The
/// elapsed-since-last-refill is measured on `tokio::time::Instant`, which
/// `tokio::time::advance` moves under a paused runtime (CLAUDE.md: behaviour
/// rides `tokio::time` directly — there is no separate fake clock). So a
/// `start_paused` test that advances 1 s sees exactly `rate_per_sec` tokens
/// refill, deterministically, with no real sleeping.
#[derive(Debug)]
pub(super) struct TokenBucket {
    /// Current token count, in `[0, capacity]`.
    tokens: f64,
    capacity: f64,
    rate_per_sec: f64,
    last_refill: tokio::time::Instant,
}

impl TokenBucket {
    /// Build a full bucket (`tokens == capacity`) refilling at `rate_per_sec`.
    pub(super) fn new(capacity: u32, rate_per_sec: u32) -> Self {
        Self {
            tokens: capacity as f64,
            capacity: capacity as f64,
            rate_per_sec: rate_per_sec as f64,
            last_refill: tokio::time::Instant::now(),
        }
    }

    /// Accrue tokens for the time elapsed since the last refill (capped at
    /// `capacity`). A no-op when no time has passed (paused clock between
    /// advances).
    fn refill(&mut self) {
        let now = tokio::time::Instant::now();
        let elapsed_sec = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed_sec <= 0.0 {
            return;
        }
        self.tokens = self.capacity.min(self.tokens + elapsed_sec * self.rate_per_sec);
        self.last_refill = now;
    }

    /// Try to consume one token after a refill. `Ok` decrements; `Err` leaves
    /// the bucket untouched and carries the seconds until a token, computed
    /// from the same refill as the failed consume, so it is always ≥ 1. With a
    /// zero refill rate the hint is `60`, a finite Retry-After for a
    /// misconfigured `rate == 0`.
    pub(super) fn try_consume(&mut self) -> Result<(), u32> {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        if self.rate_per_sec <= 0.0 {
            return Err(60);
        }
        // `tokens < 1` here, so the quotient is positive and its ceiling ≥ 1.
        Err(((1.0 - self.tokens) / self.rate_per_sec).ceil() as u32)
    }

    /// Current level, after a refill.
    pub(super) fn level(&mut self) -> f64 {
        self.refill();
        self.tokens
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::*;

    /// A consume on an empty bucket leaves it at `0`, never below, and reads
    /// the configured capacity on a fresh bucket.
    #[tokio::test(start_paused = true)]
    async fn token_bucket_never_goes_below_zero() {
        let mut b = TokenBucket::new(1, 0);
        assert_eq!(b.level(), 1.0);
        assert_eq!(b.try_consume(), Ok(()));
        assert_eq!(b.try_consume(), Err(60));
        assert_eq!(b.level(), 0.0);
    }

    /// A failed consume's Retry-After comes from the refill that failed it: a
    /// bucket a hair short of a token hints 1 s, not 0, though the token
    /// accrues right after the reject.
    #[tokio::test(start_paused = true)]
    async fn a_failed_consume_never_hints_retry_now() {
        let mut b = TokenBucket::new(1, 1000);
        assert_eq!(b.try_consume(), Ok(()));
        tokio::time::advance(std::time::Duration::from_micros(999)).await;
        assert_eq!(b.try_consume(), Err(1), "0.999 of a token is not a token");
        tokio::time::advance(std::time::Duration::from_micros(2)).await;
        assert_eq!(b.try_consume(), Ok(()), "the token accrued after the reject");
    }
}
