//! [`LinkRate`] — a byte rate on the wires of a simulated fabric, as a slow
//! TCP link: a frame's bytes leave at the rate, behind every byte queued
//! before them; a `send` blocks while more than the sender's buffer is queued
//! ahead of its frame; and a frame lands once its last byte has left, plus
//! the transit delay.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// The byte rate of every wire direction of a fabric, and the size of each
/// sender's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkRate {
    /// Bytes a wire direction carries per second (coerced to `>= 1`).
    pub bytes_per_sec: u64,
    /// Bytes a sender's buffer holds: a `send` returns once what is queued
    /// ahead of its frame fits in it.
    pub buffer_bytes: u64,
}

/// One wire direction's link: the instant its last queued byte leaves.
pub(super) struct Link {
    rate: LinkRate,
    drained_at: Mutex<Instant>,
}

impl Link {
    pub(super) fn new(rate: LinkRate) -> Self {
        Self { rate, drained_at: Mutex::new(Instant::now()) }
    }

    /// How long `bytes` take to leave.
    fn time_of(&self, bytes: u64) -> Duration {
        let nanos = u128::from(bytes) * 1_000_000_000 / u128::from(self.rate.bytes_per_sec.max(1));
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// Queue a frame of `bytes`: wait while more than the buffer is queued
    /// ahead of it, then queue it and return the instant its last byte
    /// leaves. Cancelled while waiting, it queues nothing.
    pub(super) async fn queue(&self, bytes: usize) -> Instant {
        let ahead = *self.drained_at.lock().unwrap();
        let buffer = self.time_of(self.rate.buffer_bytes);
        if let Some(room_at) = ahead.checked_sub(buffer).filter(|at| *at > Instant::now()) {
            tokio::time::sleep_until(room_at).await;
        }
        let mut drained_at = self.drained_at.lock().unwrap();
        let start = (*drained_at).max(Instant::now());
        *drained_at = start + self.time_of(u64::try_from(bytes).unwrap_or(u64::MAX));
        *drained_at
    }
}

#[cfg(test)]
mod tests;
