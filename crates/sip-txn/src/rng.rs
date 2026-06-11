//! Identifier generation seam — the port of `MessageHelpers.ts`
//! `newTag`/`newBranch` (deferred from the message slice; MIGRATION_STATUS §1
//! "Un-ported: MessageHelpers-random.test.ts").
//!
//! The source read a fiber-local Effect `Random`. We mirror the clock seam's
//! shape: a small **injectable value** (not a trait) — `IdGen::seeded(seed)`
//! for deterministic tests, `IdGen::from_entropy()` in production. The
//! transaction layer needs only `new_tag` (UAS To-tag fabricated on CANCEL
//! before any 1xx, RFC 3261 §17.2.1) and `new_branch` (fallback when an
//! outbound request carries no Via branch).
//!
//! Identifier generation is a rare, non-behavioural path; statistical
//! uniqueness from a per-process xorshift is sufficient and keeps the crate
//! free of a `rand` dependency. Determinism is the property tests care about,
//! and that is recovered by seeding.

use std::sync::atomic::{AtomicU64, Ordering};

/// Branch identifiers MUST start with this magic cookie (RFC 3261 §8.1.1.7).
const MAGIC_COOKIE: &str = "z9hG4bK";

/// A seedable identifier generator. Cheap to clone the `Arc`; the internal
/// state advances atomically so concurrent callers never collide.
#[derive(Debug)]
pub struct IdGen {
    state: AtomicU64,
}

impl IdGen {
    /// Deterministic generator — same seed yields the same id sequence. Used
    /// by tests that assert on generated identifiers.
    pub fn seeded(seed: u64) -> Self {
        // Avoid the xorshift fixed point at 0.
        Self { state: AtomicU64::new(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }) }
    }

    /// Production generator, seeded with per-process OS entropy at construction.
    ///
    /// `RandomState` is seeded once per process from the OS RNG, so two pods that
    /// start in the same clock-nanosecond still get DISTINCT id streams. The old
    /// `SystemTime`-nanos-only seed collided under coarse clocks (WSL2/VM), and
    /// because all b-leg traffic is masqueraded behind one LB VIP (identical
    /// sent-by at the peer), two workers emitting the same Via-branch / To-tag
    /// sequence would have their transactions merged at the common UAS. We still
    /// fold in the wall clock + PID so a single process's seed is also time-varying.
    pub fn from_entropy() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x1234_5678_9ABC_DEF0);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(nanos);
        h.write_u64(std::process::id() as u64);
        Self::seeded(h.finish() ^ 0xD1B5_4A32_D192_ED03)
    }

    /// xorshift64* — one step of state, returns a well-mixed `u64`.
    fn next_u64(&self) -> u64 {
        // CAS loop so two threads stepping concurrently each get a distinct
        // value (matters for branch uniqueness under the actor's send API).
        loop {
            let cur = self.state.load(Ordering::Relaxed);
            let mut x = cur;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            if self
                .state
                .compare_exchange_weak(cur, x, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            }
        }
    }

    /// RFC 3261 From/To tag — 8 base-36 chars (port of `newTag`).
    pub fn new_tag(&self) -> String {
        to_base36(self.next_u64(), 8)
    }

    /// RFC 3261 Via branch with the mandatory magic cookie (port of
    /// `newBranch`): `z9hG4bK` + 16 hex chars.
    pub fn new_branch(&self) -> String {
        format!("{MAGIC_COOKIE}{:016x}", self.next_u64())
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

    #[test]
    fn tag_is_eight_base36_chars() {
        let t = IdGen::seeded(1).new_tag();
        assert_eq!(t.len(), 8);
        assert!(t.bytes().all(|c| c.is_ascii_alphanumeric()));
    }
}
