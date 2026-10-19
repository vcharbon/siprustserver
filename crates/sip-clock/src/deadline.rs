//! Deadline arithmetic (ADR-0051 §2): `Instant + Duration` panics on an
//! instant the platform cannot represent, so a deadline a configured or
//! received duration sets is computed here, at most [`MAX_DELAY`] out.

use std::time::Duration;

use tokio::time::Instant;

/// The furthest a deadline lies: past it, a configured duration of a year or
/// more, or a deadline read off a skewed clock, is held here. It is below the
/// range of the timer wheel (2^36 ms, about 2.2 years).
pub const MAX_DELAY: Duration = Duration::from_secs(365 * 24 * 3600);

/// The instant `delay` after `from`, at most [`MAX_DELAY`] after it, and at
/// most the furthest instant the platform represents: past it the deadline
/// saturates there.
pub fn after(from: Instant, delay: Duration) -> Instant {
    let mut at = from;
    let mut left = delay.min(MAX_DELAY);
    let mut step = left;
    // Each step that does not fit is halved, each that fits is taken, so the
    // walk ends at `from + delay` or at the last instant short of it.
    while !left.is_zero() && !step.is_zero() {
        match at.checked_add(step) {
            Some(next) => {
                at = next;
                left = left.saturating_sub(step);
                step = step.min(left);
            }
            None => step /= 2,
        }
    }
    at
}

/// The instant `delay` from now, at most [`MAX_DELAY`] out.
pub fn from_now(delay: Duration) -> Instant {
    after(Instant::now(), delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_delay_past_the_ceiling_is_held_at_it() {
        let now = Instant::now();
        assert_eq!(after(now, Duration::MAX), now + MAX_DELAY);
        assert_eq!(after(now, Duration::from_millis(5)), now + Duration::from_millis(5));
    }
}
