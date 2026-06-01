//! Shared test helpers for the media integration tests.

use std::time::Duration;

/// Advance the paused tokio clock at media (sub-ptime) granularity.
///
/// A paced RTP sender sleeps one ptime (20 ms) between frames. Advancing past
/// many such deadlines in one big step starves the loop (only the first wake
/// is observed before the executor moves on) — the media-timescale version of
/// CLAUDE.md's "drive the protocol between advances." So we step in small
/// chunks and `yield_now` between them, letting the sender, the simulated-net
/// delivery tasks, and the inbound recorder all make progress each step.
pub async fn advance_media(total: Duration) {
    let step = Duration::from_millis(10);
    let mut remaining = total;
    while !remaining.is_zero() {
        let s = remaining.min(step);
        tokio::time::advance(s).await;
        tokio::task::yield_now().await;
        remaining -= s;
    }
}
