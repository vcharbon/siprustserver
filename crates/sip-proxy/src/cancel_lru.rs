//! [`CancelBranchLru`] — proxy-local `(Call-ID, CSeq number) → {target, branch}`
//! cache with TTL (port of `CancelBranchLru.ts`).
//!
//! RFC 3261 §16.10 / §17.2.3: a stateless proxy must forward a CANCEL to the
//! same downstream the matching INVITE went to, **and** reuse the INVITE's
//! outbound top-Via branch so the downstream transaction layer correlates them.
//! Keying on `(Call-ID, CSeq number)` (RFC 3261 §9.1) is the canonical
//! correlator — it works at any hop regardless of whether the upstream rewrote
//! the branch, and (unlike keying on the proxy's outbound branch) survives the
//! LoadBalancer re-sharding a fallback selection to a different worker.
//!
//! The same cache also drives hop-by-hop ACK absorption (the proxy synthesized
//! the ACK for a non-2xx final on the response path) and the non-2xx ACK
//! synthesis itself.
//!
//! Reads are O(1) and lock-only (never block on I/O). Eviction is lazy on
//! lookup plus an optional periodic [`sweep_expired`](CancelBranchLru::sweep_expired)
//! the owner task can drive.

use std::collections::HashMap;
use std::sync::Mutex;

use sip_clock::Clock;

use crate::addr::ProxyAddr;

/// Default per-entry TTL — Timer C reaches 3 min, but at the proxy hop what
/// matters is that a user-driven CANCEL in the first seconds of ringing finds
/// the right downstream; 32 s covers that comfortably.
pub const DEFAULT_TTL_MS: u64 = 32_000;
/// Default sweep cadence — half the TTL keeps the map near 1× working set.
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 16_000;

/// Build the composite key. `|` is illegal inside an RFC 3261 Call-ID `word`,
/// so the join is unambiguous.
pub fn call_id_cseq_key(call_id: &str, cseq_num: u32) -> String {
    format!("{call_id}|{cseq_num}")
}

/// What we cache per remembered INVITE: the downstream target + the branch we
/// stamped on our outgoing Via (reused on the matching CANCEL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEntry {
    pub target: ProxyAddr,
    pub branch: String,
}

struct StoredEntry {
    target: ProxyAddr,
    branch: String,
    expires_at_ms: u64,
}

/// The TTL cache. Cheap to share behind an `Arc`.
pub struct CancelBranchLru {
    table: Mutex<HashMap<String, StoredEntry>>,
    ttl_ms: u64,
    clock: Clock,
}

impl CancelBranchLru {
    /// Default TTL (32 s), system clock.
    pub fn new() -> Self {
        Self::with_opts(DEFAULT_TTL_MS, Clock::system())
    }

    /// Explicit TTL + clock (tests use `Clock::test_at(..)` for deterministic
    /// eviction under `tokio::time`).
    pub fn with_opts(ttl_ms: u64, clock: Clock) -> Self {
        Self { table: Mutex::new(HashMap::new()), ttl_ms, clock }
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Remember the downstream target + outbound branch used on an INVITE.
    pub fn remember(&self, key: &str, entry: CancelEntry) {
        let expires_at_ms = self.now_ms() + self.ttl_ms;
        self.table.lock().unwrap().insert(
            key.to_string(),
            StoredEntry { target: entry.target, branch: entry.branch, expires_at_ms },
        );
    }

    /// Look up a remembered entry (for a CANCEL / ACK). Lazily evicts an expired
    /// entry and returns `None` for it.
    pub fn lookup(&self, key: &str) -> Option<CancelEntry> {
        let now = self.now_ms();
        let mut table = self.table.lock().unwrap();
        match table.get(key) {
            Some(e) if e.expires_at_ms <= now => {
                table.remove(key);
                None
            }
            Some(e) => Some(CancelEntry { target: e.target.clone(), branch: e.branch.clone() }),
            None => None,
        }
    }

    /// Current map size — tests/metrics.
    pub fn size(&self) -> usize {
        self.table.lock().unwrap().len()
    }

    /// Drop all expired entries; returns the count swept. The owner task calls
    /// this periodically.
    pub fn sweep_expired(&self) -> usize {
        let now = self.now_ms();
        let mut table = self.table.lock().unwrap();
        let before = table.len();
        table.retain(|_, e| e.expires_at_ms > now);
        before - table.len()
    }
}

impl Default for CancelBranchLru {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(branch: &str) -> CancelEntry {
        CancelEntry { target: ProxyAddr::new("10.0.0.2", 5070), branch: branch.to_string() }
    }

    #[test]
    fn key_format_is_callid_pipe_cseq() {
        assert_eq!(call_id_cseq_key("abc@h", 7), "abc@h|7");
    }

    #[test]
    fn remember_then_lookup_returns_entry() {
        let lru = CancelBranchLru::with_opts(1000, Clock::test_at(0));
        let k = call_id_cseq_key("call-1", 1);
        lru.remember(&k, entry("z9hG4bK-1"));
        assert_eq!(lru.lookup(&k).unwrap().branch, "z9hG4bK-1");
        assert_eq!(lru.size(), 1);
        assert!(lru.lookup("absent").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_ttl() {
        let lru = CancelBranchLru::with_opts(1000, Clock::test_at(0));
        let k = call_id_cseq_key("call-1", 1);
        lru.remember(&k, entry("b"));
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        assert!(lru.lookup(&k).is_none(), "entry should have expired");
        assert_eq!(lru.sweep_expired(), 0, "lazy lookup already evicted it");
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_drops_expired() {
        let lru = CancelBranchLru::with_opts(1000, Clock::test_at(0));
        lru.remember(&call_id_cseq_key("c1", 1), entry("a"));
        lru.remember(&call_id_cseq_key("c2", 1), entry("b"));
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        assert_eq!(lru.sweep_expired(), 2);
        assert_eq!(lru.size(), 0);
    }
}
