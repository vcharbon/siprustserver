//! Quiesce-aware drain: wait for a node's live work to clear, bounded by a
//! grace deadline. Replaces a blind fixed-duration sleep on SIGTERM — a node
//! with no live calls exits at once; a busy node is capped at `grace` and its
//! residual is reported, never silently cut without a number.
//!
//! The single input is an `active()` count closure (the b2bua passes
//! [`B2buaCore::active_calls`](crate::b2bua_core::B2buaCore::active_calls)).
//! The interface is one async function over that closure — the poll loop, the
//! deadline arithmetic, and the immediate-return shortcut all live behind it,
//! so the runner's shutdown path stays a single call. Rides `tokio::time`
//! directly, so `#[tokio::test(start_paused = true)]` drives it exactly like
//! every other behavioural timer in the tree.

use std::time::Duration;

/// How often the drain re-reads `active()` while waiting. Small enough that a
/// node which clears its last call mid-grace exits promptly, not at the next
/// big tick.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// Latch nothing here — the caller has already moved the node to `Draining`
/// (so the proxy is steering new calls away). This only *waits*: it returns the
/// instant `active()` reports `0`, or when `grace` elapses, whichever is first.
///
/// Returns the residual active-call count: `0` ⇒ fully quiesced (clean
/// shutdown); `>0` ⇒ the grace deadline was hit with calls still live (the
/// caller logs the number so a too-short grace is visible, not silent).
pub async fn drain_until_quiescent<F: Fn() -> usize>(active: F, grace: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        let n = active();
        if n == 0 {
            return 0;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return n;
        }
        // Never overshoot the deadline: the last wait is clamped so a node
        // that won't quiesce returns its residual *at* the grace, not a poll
        // interval late.
        let wait = DRAIN_POLL.min(deadline - now);
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn returns_immediately_when_already_idle() {
        // No live calls ⇒ a clean node must not burn any of its grace.
        let start = tokio::time::Instant::now();
        let residual = drain_until_quiescent(|| 0, Duration::from_secs(30)).await;
        assert_eq!(residual, 0);
        assert_eq!(start.elapsed(), Duration::ZERO, "idle node should not wait");
    }

    #[tokio::test(start_paused = true)]
    async fn exits_early_the_moment_calls_clear() {
        // Three calls that drop to zero a quarter-second in: the drain must
        // return ~then, well inside the 30 s grace — not wait the full grace.
        let n = Arc::new(AtomicUsize::new(3));
        let n2 = n.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            n2.store(0, Ordering::SeqCst);
        });
        let start = tokio::time::Instant::now();
        let residual =
            drain_until_quiescent(move || n.load(Ordering::SeqCst), Duration::from_secs(30)).await;
        assert_eq!(residual, 0);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "should exit ~when calls cleared, got {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn returns_residual_and_stops_at_the_grace_when_calls_never_clear() {
        // A wedged call that never clears: the drain is bounded by the grace
        // and hands back the residual count (the caller logs it).
        let grace = Duration::from_secs(5);
        let start = tokio::time::Instant::now();
        let residual = drain_until_quiescent(|| 2, grace).await;
        assert_eq!(residual, 2);
        // Bounded *at* the grace — not a poll interval past it.
        assert_eq!(start.elapsed(), grace);
    }
}
