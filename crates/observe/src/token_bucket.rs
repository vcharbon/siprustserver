//! The trace-activation token bucket.
//!
//! Bounds how fast traces may START, independently of how many run at once: a
//! burst of `burst` activations is absorbed, then activations are paced at
//! `refill_per_sec`. Time is passed in as `now_ms` — the bucket owns no clock,
//! so a paused-clock test drives it exactly like production does.

use std::sync::Mutex;

/// Activation burst the default bucket absorbs.
pub const DEFAULT_BURST: f64 = 10.0;

/// Steady-state activations per second the default bucket allows.
pub const DEFAULT_REFILL_PER_SEC: f64 = 1.0;

struct State {
    tokens: f64,
    last_ms: i64,
}

/// A monotonic-refill token bucket. Full at construction.
pub struct TokenBucket {
    burst: f64,
    refill_per_sec: f64,
    state: Mutex<State>,
}

impl TokenBucket {
    /// A bucket holding `burst` tokens, refilled at `refill_per_sec`, starting
    /// full at `now_ms`.
    pub fn new(burst: f64, refill_per_sec: f64, now_ms: i64) -> Self {
        Self {
            burst,
            refill_per_sec,
            state: Mutex::new(State { tokens: burst, last_ms: now_ms }),
        }
    }

    /// The ADR-0026 default: burst 10, refill 1/s.
    pub fn default_at(now_ms: i64) -> Self {
        Self::new(DEFAULT_BURST, DEFAULT_REFILL_PER_SEC, now_ms)
    }

    /// Take one token, refilling for the elapsed time first. `false` when the
    /// bucket is empty. A `now_ms` that moves backwards (a wall-clock step)
    /// refills nothing and never adds tokens: the anchor is a high-water mark,
    /// so returning to an already-credited instant mints nothing a second time.
    pub fn try_take(&self, now_ms: i64) -> bool {
        let mut st = self.state.lock().expect("token bucket mutex");
        let elapsed_ms = (now_ms - st.last_ms).max(0) as f64;
        st.last_ms = st.last_ms.max(now_ms);
        st.tokens = (st.tokens + elapsed_ms / 1000.0 * self.refill_per_sec).min(self.burst);
        if st.tokens >= 1.0 {
            st.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Tokens currently available (post-refill accounting is deferred to the
    /// next [`try_take`](Self::try_take); this reads the stored level).
    pub fn tokens(&self) -> f64 {
        self.state.lock().expect("token bucket mutex").tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_burst_is_absorbed_then_the_bucket_is_empty() {
        let b = TokenBucket::default_at(0);
        for i in 0..10 {
            assert!(b.try_take(0), "burst token {i} must be granted");
        }
        assert!(!b.try_take(0), "the 11th activation in the same instant is refused");
    }

    #[test]
    fn refill_paces_activations_at_one_per_second() {
        let b = TokenBucket::default_at(0);
        for _ in 0..10 {
            assert!(b.try_take(0));
        }
        assert!(!b.try_take(999), "less than a second buys no token");
        assert!(b.try_take(1000), "one second buys exactly one token");
        assert!(!b.try_take(1000));
    }

    #[test]
    fn refill_never_exceeds_the_burst() {
        let b = TokenBucket::default_at(0);
        for _ in 0..10 {
            assert!(b.try_take(0));
        }
        // An hour idle refills to the cap, not beyond it.
        for i in 0..10 {
            assert!(b.try_take(3_600_000), "post-idle token {i} must be granted");
        }
        assert!(!b.try_take(3_600_000), "the bucket caps at the burst");
    }

    #[test]
    fn a_backwards_clock_step_grants_no_tokens() {
        let b = TokenBucket::new(1.0, 1.0, 10_000);
        assert!(b.try_take(10_000));
        assert!(!b.try_take(0), "time going backwards must not mint a token");
        assert!(!b.try_take(0));
    }

    #[test]
    fn a_step_back_and_forward_re_mints_nothing_for_time_already_credited() {
        let b = TokenBucket::new(1.0, 1.0, 0);
        assert!(b.try_take(10_000), "the starting token");
        assert!(!b.try_take(0), "a step backwards refills nothing");
        assert!(
            !b.try_take(10_000),
            "returning to an instant already credited must not buy a second token",
        );
        assert!(b.try_take(11_000), "a second of genuinely new time buys one");
    }
}
