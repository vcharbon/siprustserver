//! Rendezvous (Highest-Random-Weight, HRW) hashing — port of
//! `strategies/RendezvousHash.ts`.
//!
//! Pure function. Given a routing key (the SIP Call-ID) and a list of candidate
//! workers, return the candidate maximising `weight * H(key + ":" + id)`. With
//! consistent weights this gives O(N) lookup, no virtual nodes, and 1/N
//! expected key churn on membership change.
//!
//! `SHA-1` is **not** a cryptographic primitive here — only a fast,
//! well-distributed hash. We interpret the top 8 bytes (big-endian) of
//! `SHA1(key:id)` as a `u64`, then multiply by the weight in `u128` space (the
//! TS port used `bigint` to dodge the 2^53 float-precision loss; `u128` is the
//! exact Rust equivalent for weights up to `u64::MAX`).

use sha1::{Digest, Sha1};

/// A worker that can be picked by HRW. `weight` defaults to 1 via [`weight`].
pub trait RendezvousCandidate {
    fn id(&self) -> &str;
    /// Optional weight; higher → proportionally more keys land here.
    fn weight(&self) -> Option<u32> {
        None
    }
}

/// Top 64 bits of `SHA1(key:id)` as a `u64`.
fn score64(key: &str, id: &str) -> u64 {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b":");
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let mut acc: u64 = 0;
    for &b in digest.iter().take(8) {
        acc = (acc << 8) | u64::from(b);
    }
    acc
}

/// Pick the candidate with the highest weighted score for `key`. Stable on
/// ties via candidate-slice order (strict `>` keeps the first winner), so the
/// result is deterministic across snapshots that order workers identically.
/// Returns `None` only when `candidates` is empty.
pub fn rendezvous_select<'a, T: RendezvousCandidate>(key: &str, candidates: &'a [T]) -> Option<&'a T> {
    let mut best: Option<&T> = None;
    let mut best_score: u128 = 0;
    for c in candidates {
        let w = u128::from(c.weight().unwrap_or(1).max(1));
        let s = u128::from(score64(key, c.id())) * w;
        if best.is_none() || s > best_score {
            best = Some(c);
            best_score = s;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    struct W(&'static str);
    impl RendezvousCandidate for W {
        fn id(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn empty_yields_none() {
        let cs: Vec<W> = vec![];
        assert!(rendezvous_select("call-1", &cs).is_none());
    }

    #[test]
    fn deterministic_regardless_of_order() {
        let a = [W("w1"), W("w2"), W("w3")];
        let b = [W("w3"), W("w1"), W("w2")];
        let wa = rendezvous_select("call-abc", &a).unwrap().0;
        let wb = rendezvous_select("call-abc", &b).unwrap().0;
        assert_eq!(wa, wb);
    }

    #[test]
    fn distributes_across_workers_within_tolerance() {
        // Port of load-balancer/distribution.test.ts: 1000 keys over 8 workers,
        // each worker within ±25% of the 125 mean (94..=156).
        let workers: Vec<W> = (1..=8)
            .map(|i| W(Box::leak(format!("b2b-{i}").into_boxed_str()) as &'static str))
            .collect();
        let mut counts = std::collections::HashMap::new();
        for i in 0..1000usize {
            // Deterministic LCG, mirroring the source's seeded PRNG, for stable Call-IDs.
            let key = format!("call-{}-distribute@host", i.wrapping_mul(2_654_435_761usize));
            let w = rendezvous_select(&key, &workers).unwrap();
            *counts.entry(w.0).or_insert(0u32) += 1;
        }
        let total: u32 = counts.values().sum();
        assert_eq!(total, 1000);
        for w in &workers {
            let c = *counts.get(w.0).unwrap_or(&0);
            assert!((94..=156).contains(&c), "worker {} got {c} (out of ±25% band)", w.0);
        }
    }

    #[test]
    fn weight_biases_selection() {
        // A heavily-weighted worker should win a large majority of keys.
        struct Wt(&'static str, u32);
        impl RendezvousCandidate for Wt {
            fn id(&self) -> &str {
                self.0
            }
            fn weight(&self) -> Option<u32> {
                Some(self.1)
            }
        }
        let cs = [Wt("light", 1), Wt("heavy", 1_000_000)];
        let mut heavy = 0;
        for i in 0..200 {
            if rendezvous_select(&format!("k{i}"), &cs).unwrap().0 == "heavy" {
                heavy += 1;
            }
        }
        assert!(heavy > 180, "heavy won only {heavy}/200");
    }
}
