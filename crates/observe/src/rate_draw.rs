//! The Bernoulli sampling draw.
//!
//! A lock-free xorshift64* stream: one relaxed CAS-free fetch per draw, no
//! allocation, and fully reproducible from a seed so admission tests are
//! deterministic. It is a sampling die, not a cryptographic source.

use std::sync::atomic::{AtomicU64, Ordering};

/// Seedable pseudo-random Bernoulli draw.
pub struct RateDraw {
    state: AtomicU64,
}

impl RateDraw {
    /// A draw stream reproducing exactly the same sequence for the same seed.
    /// A zero seed is remapped — xorshift is absorbing at 0.
    pub fn seeded(seed: u64) -> Self {
        Self { state: AtomicU64::new(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }) }
    }

    /// A draw stream seeded from process/time entropy.
    pub fn from_entropy() -> Self {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5DEE_CE66);
        Self::seeded(t ^ ((std::process::id() as u64) << 32))
    }

    /// `true` with probability `rate`. A rate at or below 0 never draws, at or
    /// above 1 always draws, and a non-finite rate never draws.
    pub fn draw(&self, rate: f64) -> bool {
        if !rate.is_finite() || rate <= 0.0 {
            return false;
        }
        if rate >= 1.0 {
            return true;
        }
        self.next_unit() < rate
    }

    /// The next value in `[0, 1)`.
    fn next_unit(&self) -> f64 {
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);
        // Top 53 bits — the exact mantissa width of an f64 in [0, 1).
        ((x >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

impl Default for RateDraw {
    fn default() -> Self {
        Self::from_entropy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_rates_are_absolute() {
        let d = RateDraw::seeded(7);
        for _ in 0..1000 {
            assert!(!d.draw(0.0));
            assert!(!d.draw(-1.0));
            assert!(!d.draw(f64::NAN));
            assert!(d.draw(1.0));
            assert!(d.draw(2.0));
        }
    }

    #[test]
    fn the_same_seed_reproduces_the_same_sequence() {
        let a = RateDraw::seeded(42);
        let b = RateDraw::seeded(42);
        let seq_a: Vec<bool> = (0..64).map(|_| a.draw(0.5)).collect();
        let seq_b: Vec<bool> = (0..64).map(|_| b.draw(0.5)).collect();
        assert_eq!(seq_a, seq_b);
    }

    #[test]
    fn the_draw_tracks_the_configured_rate() {
        let d = RateDraw::seeded(0xC0FF_EE);
        const N: usize = 200_000;
        let hits = (0..N).filter(|_| d.draw(0.05)).count();
        let observed = hits as f64 / N as f64;
        assert!((observed - 0.05).abs() < 0.005, "observed rate {observed} is not near 0.05");
    }

    #[test]
    fn the_default_rate_draws_rarely_but_not_never() {
        let d = RateDraw::seeded(1234);
        const N: usize = 2_000_000;
        let hits = (0..N).filter(|_| d.draw(crate::DEFAULT_SAMPLE_RATE)).count();
        assert!((100..=400).contains(&hits), "1e-4 over {N} draws gave {hits} hits");
    }
}
