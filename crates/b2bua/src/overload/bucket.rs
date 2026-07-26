//! Hard CPS gate: a lazy-refill token bucket riding `tokio::time::Instant`.

/// Lazy-refill token bucket. Tokens accrue continuously at `rate_per_sec` up to
/// `capacity`; [`try_consume`](TokenBucket::try_consume) succeeds iff at least
/// one token is available.
///
/// **Clock — rides `tokio::time::Instant`, not wall time.** The
/// elapsed-since-last-refill is measured on `tokio::time::Instant`, which
/// `tokio::time::advance` moves under a paused runtime (CLAUDE.md: behaviour
/// rides `tokio::time` directly — there is no separate fake clock). So a
/// `start_paused` test that advances 1 s sees exactly `rate_per_sec` tokens
/// refill, deterministically, with no real sleeping.
#[derive(Debug)]
pub(super) struct TokenBucket {
    /// Current token count. May go negative (the emergency `consume_forced` path).
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

    /// Try to consume one token. Returns `true` (and decrements) iff ≥ 1 is
    /// available after a refill; `false` leaves the bucket untouched.
    pub(super) fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Consume one token unconditionally — the level may go negative. The
    /// emergency path uses this so the bucket still reflects true CPS load (a
    /// burst of emergency calls makes subsequent non-emergency callers wait
    /// longer for refill).
    pub(super) fn consume_forced(&mut self) {
        self.refill();
        self.tokens -= 1.0;
    }

    /// Seconds until ≥ 1 token will be available (`0` if available now). With a
    /// zero refill rate and an empty bucket, returns `60` — a misconfigured
    /// `rate == 0` still hands the caller a finite Retry-After.
    pub(super) fn retry_after_sec(&mut self) -> u32 {
        self.refill();
        if self.tokens >= 1.0 {
            return 0;
        }
        if self.rate_per_sec <= 0.0 {
            return 60;
        }
        ((1.0 - self.tokens) / self.rate_per_sec).ceil() as u32
    }

    /// Current level, floored at `0` (the negative emergency overdraft reads as
    /// empty to an observer).
    pub(super) fn level(&mut self) -> f64 {
        self.refill();
        self.tokens.max(0.0)
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::*;

    /// `level()` floors the (possibly negative) overdraft at 0 for an observer,
    /// and reads the configured capacity on a fresh bucket.
    #[tokio::test(start_paused = true)]
    async fn token_bucket_level_floors_at_zero() {
        let mut b = TokenBucket::new(3, 0);
        assert_eq!(b.level(), 3.0);
        b.consume_forced();
        b.consume_forced();
        b.consume_forced();
        b.consume_forced(); // -1 internally
        assert_eq!(b.level(), 0.0, "a negative overdraft reads as empty");
    }
}
