//! Probabilistic packet loss — the network layer's random-loss primitive.
//!
//! Random loss is a property of the FABRIC, so its model lives here; harnesses
//! (e.g. the loadgen mux) apply it per endpoint, in both directions, including
//! on their own retransmits. Deterministic, message-targeted drops are a test
//! concern and live with the harness that needs them, NOT here.

use std::sync::atomic::{AtomicU64, Ordering};

/// Independent per-datagram loss with probability `rate`, driven by a seeded
/// xorshift64 RNG so a run is reproducible from its seed. `rate <= 0.0`
/// disables it: [`drops`](Self::drops) short-circuits with no RNG churn.
///
/// The RNG state is advanced with relaxed atomics; a rare same-value race under
/// concurrent send/recv is statistically irrelevant for a loss model.
#[derive(Debug)]
pub struct RandomLoss {
    rate: f64,
    state: AtomicU64,
}

impl RandomLoss {
    /// `seed` is forced non-zero (xorshift's absorbing state).
    pub fn new(rate: f64, seed: u64) -> Self {
        Self { rate, state: AtomicU64::new(seed | 1) }
    }

    /// Whether THIS datagram is dropped. `false` (no RNG advance) when disabled.
    pub fn drops(&self) -> bool {
        if self.rate <= 0.0 {
            return false;
        }
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);
        (x as f64 / u64::MAX as f64) < self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_drops_and_never_advances() {
        let l = RandomLoss::new(0.0, 42);
        assert!((0..1000).all(|_| !l.drops()));
    }

    #[test]
    fn rate_is_roughly_honoured_and_seed_reproducible() {
        let a = RandomLoss::new(0.1, 7);
        let b = RandomLoss::new(0.1, 7);
        let da: Vec<bool> = (0..10_000).map(|_| a.drops()).collect();
        let db: Vec<bool> = (0..10_000).map(|_| b.drops()).collect();
        assert_eq!(da, db, "same seed → same drop sequence");
        let hits = da.iter().filter(|d| **d).count();
        assert!((500..2000).contains(&hits), "≈10% of 10k, got {hits}");
    }
}
