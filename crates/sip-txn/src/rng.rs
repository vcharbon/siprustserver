//! Identifier generation seam. Mirrors the clock seam's shape: a small
//! **injectable value** (not a trait) — `IdGen::seeded(seed)` for
//! deterministic tests, `IdGen::from_entropy()` in production.
//!
//! Every identifier is HMAC-SHA256 over a counter under a secret key, so an
//! off-path host cannot guess the branch a response is matched by: the output
//! reveals neither the key nor the next value, and two keys give unrelated
//! streams. Why the response's source is not checked: ADR-0007.

use std::sync::atomic::{AtomicU64, Ordering};

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Branch identifiers MUST start with this magic cookie (RFC 3261 §8.1.1.7).
const MAGIC_COOKIE: &str = "z9hG4bK";

/// A keyed identifier generator. Cheap to share behind an `Arc`; the counter
/// advances atomically so concurrent callers never draw the same value.
pub struct IdGen {
    /// The PRF, keyed once; each draw clones it.
    prf: Hmac<Sha256>,
    counter: AtomicU64,
}

impl std::fmt::Debug for IdGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdGen").finish_non_exhaustive()
    }
}

impl IdGen {
    /// Deterministic generator — same seed yields the same id sequence. Used
    /// by tests that assert on generated identifiers. Not for production: a
    /// 64-bit seed is a guessable key.
    pub fn seeded(seed: u64) -> Self {
        Self::keyed(&seed.to_le_bytes())
    }

    /// Production generator, keyed with 256 bits from the OS RNG, so every
    /// process — two pods behind one load-balancer address included — draws
    /// its own unpredictable stream.
    ///
    /// # Panics
    ///
    /// When the OS RNG is unavailable: identifiers would be guessable.
    pub fn from_entropy() -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS RNG keys the SIP identifier generator");
        Self::keyed(&key)
    }

    fn keyed(key: &[u8]) -> Self {
        let prf = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
        Self { prf, counter: AtomicU64::new(0) }
    }

    /// The next PRF output: HMAC(key, counter), its first 8 bytes.
    fn next_u64(&self) -> u64 {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut mac = self.prf.clone();
        mac.update(&n.to_le_bytes());
        let out = mac.finalize().into_bytes();
        u64::from_le_bytes(out[..8].try_into().expect("a SHA-256 output holds 8 bytes"))
    }

    /// A uniform 64-bit draw, for a value picked at random (a `Retry-After`).
    pub fn draw(&self) -> u64 {
        self.next_u64()
    }

    /// RFC 3261 From/To tag — 8 base-36 chars.
    pub fn new_tag(&self) -> String {
        to_base36(self.next_u64(), 8)
    }

    /// RFC 3261 Via branch with the mandatory magic cookie:
    /// `z9hG4bK` + 16 hex chars.
    pub fn new_branch(&self) -> String {
        format!("{MAGIC_COOKIE}{:016x}", self.next_u64())
    }

    /// The initial value of a 32-bit signed sequence counter, picked at random
    /// from `1..=10^8` so a peer cannot guess where a sequence resumes. The
    /// ceiling RFC 3261 §8.1.1.5 (CSeq) and RFC 3262 §7.1 (RSeq) share is
    /// `2^31 - 1`; starting two decimal orders of magnitude below it leaves
    /// every increment a long call can make inside the range.
    pub fn new_sequence_number(&self) -> u32 {
        (self.next_u64() % 100_000_000) as u32 + 1
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::from_entropy()
    }
}

fn to_base36(mut v: u64, len: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 16];
    for slot in buf.iter_mut().rev() {
        *slot = ALPHABET[(v % 36) as usize];
        v /= 36;
    }
    String::from_utf8_lossy(&buf[buf.len() - len..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_has_magic_cookie_and_is_unique() {
        let g = IdGen::seeded(42);
        let a = g.new_branch();
        let b = g.new_branch();
        assert!(a.starts_with(MAGIC_COOKIE));
        assert_ne!(a, b);
    }

    #[test]
    fn seeded_is_deterministic() {
        let a = IdGen::seeded(7);
        let b = IdGen::seeded(7);
        assert_eq!(a.new_tag(), b.new_tag());
        assert_eq!(a.new_branch(), b.new_branch());
    }

    /// The u64 a branch carries.
    fn branch_value(branch: &str) -> u64 {
        u64::from_str_radix(branch.strip_prefix(MAGIC_COOKIE).unwrap(), 16).unwrap()
    }

    /// An observer holding one branch cannot compute the next: the output
    /// is no invertible image of the generator's state. The attack tried is
    /// the one a xorshift64* stream falls to — undo the output multiply,
    /// step the state, multiply again.
    #[test]
    fn one_branch_does_not_predict_the_next() {
        const MUL: u64 = 0x2545_F491_4F6C_DD1D;
        // MUL's inverse mod 2^64 by Newton iteration (MUL is odd).
        let mut inv: u64 = MUL;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(MUL.wrapping_mul(inv)));
        }
        let predict = |seen: u64| {
            let mut x = seen.wrapping_mul(inv);
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(MUL)
        };
        for seed in [1, 7, 42, 0xB2B0] {
            let g = IdGen::seeded(seed);
            let seen = branch_value(&g.new_branch());
            let next = branch_value(&g.new_branch());
            assert_ne!(predict(seen), next, "seed {seed}: the next branch was predicted");
        }
    }

    /// Two layers seeded differently draw unrelated streams: no id of one
    /// appears anywhere in the other, the seed that a xorshift step makes of
    /// the first included.
    #[test]
    fn differently_seeded_generators_share_no_ids() {
        let step = |mut x: u64| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x
        };
        for (sa, sb) in [(1, 2), (1, step(1)), (42, step(42))] {
            let (a, b) = (IdGen::seeded(sa), IdGen::seeded(sb));
            let first: std::collections::HashSet<String> =
                (0..10_000).map(|_| a.new_branch()).collect();
            assert!(
                (0..10_000).all(|_| !first.contains(&b.new_branch())),
                "seeds {sa} and {sb} share an id"
            );
        }
    }

    /// Production keys come from the OS RNG: two generators draw different
    /// first ids, and neither draws the all-zero key's stream (HMAC pads a
    /// key with zeros, so `seeded(0)` is that stream).
    #[test]
    fn entropy_keys_are_neither_shared_nor_zero() {
        let zero = IdGen::seeded(0);
        let (zero_branch, zero_tag) = (zero.new_branch(), zero.new_tag());
        let (a, b) = (IdGen::from_entropy(), IdGen::from_entropy());
        let (a_branch, a_tag) = (a.new_branch(), a.new_tag());
        let (b_branch, b_tag) = (b.new_branch(), b.new_tag());
        assert_ne!(a_branch, b_branch);
        assert_ne!(a_tag, b_tag);
        for (branch, tag) in [(&a_branch, &a_tag), (&b_branch, &b_tag)] {
            assert_ne!(branch, &zero_branch, "a zero key");
            assert_ne!(tag, &zero_tag, "a zero key");
        }
    }

    #[test]
    fn tag_is_eight_base36_chars() {
        let t = IdGen::seeded(1).new_tag();
        assert_eq!(t.len(), 8);
        assert!(t.bytes().all(|c| c.is_ascii_alphanumeric()));
    }
}
