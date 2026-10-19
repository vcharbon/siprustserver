//! Arming a `DelayQueue` entry within its wheel's range (ADR-0051 §2):
//! `DelayQueue::insert` panics on a delay past the wheel (2^36 ms, about 2.2
//! years), so every delay a timer owner arms goes through [`insert`] or
//! [`insert_at`], which hold it at [`MAX_DELAY`].

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::time::delay_queue::{DelayQueue, Key};

pub use crate::deadline::MAX_DELAY;

/// Insert `value` into `queue`, due after `delay`, at most [`MAX_DELAY`].
#[expect(clippy::disallowed_methods, reason = "the delay is held below the wheel's range")]
pub fn insert<T>(queue: &mut DelayQueue<T>, value: T, delay: Duration) -> Key {
    queue.insert(value, delay.min(MAX_DELAY))
}

/// Insert `value` into `queue`, due at `deadline`, at most [`MAX_DELAY`]
/// from now; a deadline already past is due at once.
pub fn insert_at<T>(queue: &mut DelayQueue<T>, value: T, deadline: Instant) -> Key {
    insert(queue, value, deadline.saturating_duration_since(Instant::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_delay_past_the_wheel_arms_at_the_ceiling() {
        let mut queue = DelayQueue::new();
        let key = insert(&mut queue, (), Duration::MAX);
        assert_eq!(queue.deadline(&key), Instant::now() + MAX_DELAY);
        let far = insert_at(&mut queue, (), Instant::now() + MAX_DELAY * 3);
        assert_eq!(queue.deadline(&far), Instant::now() + MAX_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deadline_already_past_is_due_at_once() {
        let mut queue = DelayQueue::new();
        tokio::time::advance(Duration::from_secs(5)).await;
        let past = Instant::now() - Duration::from_secs(2);
        let key = insert_at(&mut queue, (), past);
        assert_eq!(queue.deadline(&key), Instant::now());
    }
}
