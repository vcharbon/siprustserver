//! Recv-shard liveness: is every shard turning its loop?
//!
//! Every activity meter the proxy has (intake age, ELU) reads zero for a
//! shard that took a packet and never came back, so a parked shard is
//! indistinguishable from an idle one. This is the one signal that tells
//! them apart: each shard stamps the instant it dequeued a packet and clears
//! the stamp when it returns to waiting on its sockets. A stamp older than
//! the stall threshold is a shard that is neither idle nor turning.

use std::sync::atomic::{AtomicU64, Ordering};

/// One cell per recv shard: `0` while the shard waits on `recv()`, else the
/// clock reading (ms) at which it dequeued the packet it is still handling.
#[derive(Debug)]
pub struct ShardPulse {
    busy_since_ms: Vec<AtomicU64>,
}

impl ShardPulse {
    pub fn new(shards: usize) -> Self {
        Self { busy_since_ms: (0..shards.max(1)).map(|_| AtomicU64::new(0)).collect() }
    }

    pub fn shards(&self) -> usize {
        self.busy_since_ms.len()
    }

    /// `shard` dequeued a packet at `now_ms` and is handling it. A reading of
    /// `0` is stored as `1`: `0` means waiting.
    pub fn took_packet(&self, shard: usize, now_ms: u64) {
        if let Some(cell) = self.busy_since_ms.get(shard) {
            cell.store(now_ms.max(1), Ordering::Relaxed);
        }
    }

    /// `shard` is back to waiting on its sockets.
    pub fn waiting(&self, shard: usize) {
        if let Some(cell) = self.busy_since_ms.get(shard) {
            cell.store(0, Ordering::Relaxed);
        }
    }

    /// The shards that dequeued a packet more than `threshold_ms` ago and have
    /// not returned to waiting since, on the same clock the stamps use.
    pub fn stalled(&self, now_ms: u64, threshold_ms: u64) -> Vec<usize> {
        self.busy_since_ms
            .iter()
            .enumerate()
            .filter(|(_, cell)| {
                let since = cell.load(Ordering::Relaxed);
                since != 0 && now_ms.saturating_sub(since) > threshold_ms
            })
            .map(|(i, _)| i)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_waiting_shard_is_never_stalled_however_long_it_waits() {
        let p = ShardPulse::new(2);
        assert!(p.stalled(1_000_000, 1).is_empty());
        p.took_packet(1, 10);
        p.waiting(1);
        assert!(p.stalled(1_000_000, 1).is_empty());
    }

    #[test]
    fn a_shard_still_on_one_packet_past_the_threshold_is_stalled() {
        let p = ShardPulse::new(4);
        p.took_packet(2, 1_000);
        assert!(p.stalled(1_900, 1_000).is_empty(), "inside the threshold");
        assert_eq!(p.stalled(2_001, 1_000), vec![2]);
        p.took_packet(2, 2_001);
        assert!(p.stalled(2_500, 1_000).is_empty(), "a fresh packet restarts the clock");
    }

    #[test]
    fn a_zero_clock_reading_still_counts_as_busy() {
        let p = ShardPulse::new(1);
        p.took_packet(0, 0);
        assert_eq!(p.stalled(5, 1), vec![0]);
    }

    #[test]
    fn an_out_of_range_shard_is_ignored() {
        let p = ShardPulse::new(1);
        p.took_packet(7, 10);
        p.waiting(7);
        assert_eq!(p.shards(), 1);
        assert!(p.stalled(100, 1).is_empty());
    }
}
