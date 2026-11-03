//! [`ErrorCloses`] — a backend client closes the connection a 5xx answer
//! came on instead of pooling it (ADR-0043).
//!
//! A pooled connection keeps the backend a layer-4 balancer chose for it, and
//! the pool hands out the most recently returned connection first: a backend
//! answering errors behind one address would keep every connection it holds,
//! and every request on them, until the client rotates. Closing the
//! connection after a 5xx makes the next request take another pooled
//! connection or a new one, which the balancer spreads again. The request is
//! never sent again (the retry rule stays: never after an HTTP answer).
//!
//! The closes are capped at `per_s` a second per client, a token bucket of
//! `per_s` tokens: at a total 5xx outage every answer would close its
//! connection, and each close leaves a socket in `TIME_WAIT` for the
//! kernel's 60 s; past the cap the connection is pooled as any other. A 200
//! whose body does not parse is the caller's to judge and pools as usual.

use std::sync::Mutex;

use tokio::time::Instant;

use crate::backend::metrics;
use crate::poison::{locked, ERROR_CLOSES};

/// One client's budget of connection closes after a 5xx answer.
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

    /// Whether the connection a `status` answer of `peer` came on is closed:
    /// a 5xx within the budget. Every 5xx is counted by what became of its
    /// connection.
    pub fn close_after(&self, peer: &str, status: u16) -> bool {
        if !(500..=599).contains(&status) {
            return false;
        }
        let closed = self.take();
        metrics::error_answered(peer, closed);
        closed
    }

    fn take(&self) -> bool {
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
    async fn only_a_5xx_closes_within_the_budget_refilled_each_second() {
        let c = ErrorCloses::new(2);
        assert!(!c.close_after("p", 200));
        assert!(!c.close_after("p", 404));
        assert!(c.close_after("p", 500));
        assert!(c.close_after("p", 503));
        assert!(!c.close_after("p", 502), "past the budget the connection pools");
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(c.close_after("p", 500), "half a second refills one");
        assert!(!c.close_after("p", 500));
        assert_eq!(metrics::error_answers("p", true), 3);
        assert_eq!(metrics::error_answers("p", false), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_never_closes() {
        assert!(!ErrorCloses::new(0).close_after("q", 500));
    }
}
