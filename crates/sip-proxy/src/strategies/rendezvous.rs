//! Rendezvous (highest-random-weight) hashing: a pure function from a
//! routing key (the SIP Call-ID) and candidate workers to the candidate
//! maximising `H(key:id)`, `H` the top 8 bytes (big-endian) of `SHA1(key:id)`
//! as a `u64` (integer, no float), which is the score. A membership change
//! moves the keys of the changed worker only. See ADR-0009 X3.

use sha1::{Digest, Sha1};

pub trait RendezvousCandidate {
    fn id(&self) -> &str;
}

fn score(key: &str, id: &str) -> u64 {
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

/// Pick the candidate with the highest score for `key`. Stable on ties via
/// candidate-slice order (strict `>` keeps the first winner), so the result
/// is deterministic across snapshots that order workers identically.
/// Returns `None` only when `candidates` is empty.
pub fn rendezvous_select<'a, T: RendezvousCandidate>(
    key: &str,
    candidates: &'a [T],
) -> Option<&'a T> {
    let mut best: Option<&T> = None;
    let mut best_score: u64 = 0;
    for c in candidates {
        let s = score(key, c.id());
        if best.is_none() || s > best_score {
            best = Some(c);
            best_score = s;
        }
    }
    best
}

/// Every candidate in descending score for `key`, ties in slice order: the
/// first is [`rendezvous_select`]'s winner, each next one the winner once
/// those before it are taken out.
pub fn rendezvous_rank<'a, T: RendezvousCandidate>(key: &str, candidates: &'a [T]) -> Vec<&'a T> {
    let mut ranked: Vec<(u64, &T)> = candidates.iter().map(|c| (score(key, c.id()), c)).collect();
    ranked.sort_by_key(|&(s, _)| std::cmp::Reverse(s));
    ranked.into_iter().map(|(_, c)| c).collect()
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
    fn rank_heads_with_the_winner_and_drops_down_by_removal() {
        let cs = [W("w1"), W("w2"), W("w3"), W("w4")];
        for i in 0..50 {
            let key = format!("call-{i}");
            let ranked: Vec<&str> = rendezvous_rank(&key, &cs).iter().map(|c| c.0).collect();
            assert_eq!(ranked.len(), cs.len());
            assert_eq!(ranked[0], rendezvous_select(&key, &cs).unwrap().0);
            let rest: Vec<W> = cs.iter().filter(|c| c.0 != ranked[0]).map(|c| W(c.0)).collect();
            assert_eq!(ranked[1], rendezvous_select(&key, &rest).unwrap().0);
        }
    }

    /// 1000 keys over 8 workers, each worker within ±25% of the 125 mean
    /// (94..=156). The Call-IDs are a deterministic multiplicative spread, so
    /// the counts are stable.
    #[test]
    fn distributes_across_workers_within_tolerance() {
        let workers: Vec<W> = (1..=8)
            .map(|i| W(Box::leak(format!("b2b-{i}").into_boxed_str()) as &'static str))
            .collect();
        let mut counts = std::collections::HashMap::new();
        for i in 0..1000usize {
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
}
