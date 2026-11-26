//! [`ErrorCloses`] — the budget of connections a backend client closes on
//! its answers ([`crate::ejection`]): `per_s` a second per client, a token
//! bucket of `per_s` tokens.

use std::sync::Mutex;

use tokio::time::Instant;

use crate::poison::{locked, ERROR_CLOSES};

/// One client's budget of connection closes on its answers.
pub struct ErrorCloses {
    per_s: u32,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl ErrorCloses {
    /// A budget of `per_s` closes a second, full now; `0` never closes.
    pub fn new(per_s: u32) -> Self {
        Self { per_s, bucket: Mutex::new(Bucket { tokens: f64::from(per_s), at: Instant::now() }) }
    }

    /// Whether one more close fits the budget now; one that fits is taken.
    pub fn take(&self) -> bool {
        if self.per_s == 0 {
            return false;
        }
        let mut b = locked(&self.bucket, ERROR_CLOSES);
        let now = Instant::now();
        let rate = f64::from(self.per_s);
        let refill = now.duration_since(b.at).as_secs_f64() * rate;
        b.tokens = (b.tokens + refill).min(rate);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The closes a second a client makes at most unless told otherwise.
pub const DEFAULT_ERROR_CLOSES_PER_S: u32 = 20;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_budget_is_full_at_first_and_refills_at_its_rate() {
        let c = ErrorCloses::new(2);
        assert_eq!([c.take(), c.take(), c.take()], [true, true, false]);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!([c.take(), c.take()], [true, false], "half a second refills one");
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!([c.take(), c.take(), c.take()], [true, true, false], "never past full");
    }

    #[tokio::test(start_paused = true)]
    async fn zero_never_closes() {
        assert!(!ErrorCloses::new(0).take());
    }
}
