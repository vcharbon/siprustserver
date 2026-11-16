//! The per-call seeded generator and the seed each call draws from.

/// SplitMix64: small, seedable, dependency-free. Two generators of one seed
/// yield the same stream on every platform.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// The generator whose stream `seed` fixes.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// The stream's next value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`; `n = 0` reads as 1, so the value is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// True `pct` times in a hundred.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    /// One element of the non-empty `xs`.
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

/// FNV-1a of `s`, its offset basis mixed with `seed`.
fn fnv(seed: u64, s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The seed a call draws from: `replay` when one call is replayed, else the
/// run's seed hashed with the call's token, so every call of a run differs and
/// a run seed reproduces them all.
pub fn call_seed(run_seed: u64, replay: Option<u64>, token: &str) -> u64 {
    replay.unwrap_or_else(|| fnv(run_seed, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_fixes_the_stream_and_the_token_the_seed() {
        let draw = |seed| {
            let mut rng = Rng::new(seed);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(42), draw(42));
        assert_eq!(
            draw(42).len(),
            draw(42).iter().collect::<std::collections::BTreeSet<_>>().len()
        );
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
        assert_eq!(call_seed(7, None, "c1"), call_seed(7, None, "c1"));
        assert_ne!(call_seed(7, None, "c1"), call_seed(7, None, "c2"));
        assert_ne!(call_seed(7, None, "c1"), call_seed(8, None, "c1"));
        assert_eq!(call_seed(7, Some(0xabc), "c1"), 0xabc);
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = Rng::new(3);
        assert!((0..1000).all(|_| rng.below(7) < 7));
        assert_eq!(rng.below(0), 0);
        assert!((0..100).all(|_| !rng.chance(0) && rng.chance(100)));
    }
}
