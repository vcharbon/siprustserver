//! [`CancelBranchLru`] — proxy-local `(Call-ID, From-tag, CSeq number) →
//! {target, branch}` cache with per-entry TTL (port of `CancelBranchLru.ts`).
//!
//! RFC 3261 §16.10 / §17.2.3: a stateless proxy must forward a CANCEL to the
//! same downstream the matching INVITE went to, **and** reuse the INVITE's
//! outbound top-Via branch so the downstream transaction layer correlates them.
//! Keying on `(Call-ID, From-tag, CSeq number)` (RFC 3261 §9.1) is the
//! canonical correlator — it works at any hop regardless of whether the
//! upstream rewrote the branch, and (unlike keying on the proxy's outbound
//! branch) survives the LoadBalancer re-sharding a fallback selection to a
//! different worker. The From-tag matters because BOTH directions of a dialog
//! are remembered here and share the Call-ID with *independent* CSeq spaces
//! (§12.2.1.1): without it, a UAC re-INVITE and a worker-outbound re-INVITE
//! that happen to land on the same CSeq number within one TTL overwrite each
//! other's entry, and the later CANCEL/ACK is forwarded to the wrong party
//! with the wrong branch.
//!
//! The same cache also drives the non-2xx ACK hop decision (`ackhop|` keys —
//! relay the upstream's §17.1.1.3 ACK on the INVITE's remembered hop, or
//! absorb it when the proxy itself generated the final; see `core/request.rs`
//! and `core/response.rs`) and the retransmission branch memo (`rtx|`-prefixed
//! keys, see `core/request.rs`).
//!
//! Reads are O(1) and lock-only (never block on I/O). Eviction is lazy on
//! lookup plus an optional periodic [`sweep_expired`](CancelBranchLru::sweep_expired)
//! the owner task can drive.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use sip_txn::timers::{INVITE_INITIAL_TIMEOUT, TIMER_F, TIMER_H};

use crate::addr::ProxyAddr;
use crate::observability::ProxyMetrics;

/// TTL for pending-INVITE entries (CANCEL forwarding + non-2xx ACK synthesis).
/// Must cover the downstream UA's **whole INVITE transaction window**: the
/// B2BUA answers or gives up within `sip-txn`'s `INVITE_INITIAL_TIMEOUT`
/// (158 s, which wraps its 150 s `SetupTimeout` ledger), plus the
/// final-response retransmit tail (Timer H). Imported from `sip-txn` so the
/// proxy's memory and the B2BUA's transaction timers cannot drift apart. The
/// old 32 s TTL made a CANCEL after half a minute of (perfectly legal) ringing
/// miss the entry and go downstream with a FRESH branch → 481 Transaction Does
/// Not Exist, while the callee kept ringing.
pub const INVITE_ENTRY_TTL_MS: u64 = INVITE_INITIAL_TIMEOUT + TIMER_H;

/// TTL for retransmission branch memos (`rtx|` keys): upstream retransmits
/// stop at Timer B/F (64×T1 = 32 s), so these need live no longer. They are
/// written for EVERY forwarded request — including each keepalive OPTIONS — so
/// keeping them short keeps the map at ≈ one transaction window of traffic.
pub const RTX_ENTRY_TTL_MS: u64 = TIMER_F;

/// Default sweep cadence — half the SHORT (rtx) TTL, so the dominant entry
/// class is physically reclaimed near its expiry and the map stays at ~1×
/// working set.
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 16_000;

const _: () = assert!(DEFAULT_SWEEP_INTERVAL_MS <= RTX_ENTRY_TTL_MS);
const _: () = assert!(RTX_ENTRY_TTL_MS <= INVITE_ENTRY_TTL_MS);

/// Build the composite key. `|` is illegal inside an RFC 3261 Call-ID `word`
/// and inside a `tag` token, so the join is unambiguous. A missing From-tag
/// (pre-RFC-3261 UA) keys as the empty string.
pub fn call_id_cseq_key(call_id: &str, from_tag: Option<&str>, cseq_num: u32) -> String {
    format!("{call_id}|{}|{cseq_num}", from_tag.unwrap_or(""))
}

/// Namespaced key for the non-2xx ACK hop memo, consulted on the request path
/// when the upstream's §17.1.1.3 ACK arrives. Written in two flavours:
///  • RESPONSE path, on relaying a non-2xx INVITE final upstream — carries
///    the INVITE's forward (target + outbound branch) so the ACK is RELAYED
///    on that exact hop (the downstream server transaction matches it and
///    stops retransmitting the final);
///  • request path `reply()`, on a final the proxy generated ITSELF — empty
///    `branch`, so the ACK is ABSORBED here (the proxy is the UAS; no
///    downstream exists).
/// `ackhop|` keeps it disjoint from the plain INVITE keys and the `rtx|`
/// memos sharing this store.
pub fn ack_hop_key(call_id: &str, from_tag: Option<&str>, cseq_num: u32) -> String {
    format!("ackhop|{}", call_id_cseq_key(call_id, from_tag, cseq_num))
}

/// What we cache per remembered INVITE: the downstream target + the branch we
/// stamped on our outgoing Via (reused on the matching CANCEL) + the branch
/// the UPSTREAM stamped on the top Via of the request as it arrived.
///
/// `upstream_branch` is the §17.1.1.3 discriminator for the non-2xx ACK hop
/// decision (consulted via the [`ack_hop_key`] memo): an ACK for a NON-2xx
/// final reuses its INVITE's top-Via branch (same transaction), while an ACK
/// for a 2xx is a new transaction with a fresh branch and takes the normal
/// routing ladder. Empty when the upstream request carried no branch
/// (pre-RFC-3261 UA) — never matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEntry {
    pub target: ProxyAddr,
    pub branch: String,
    pub upstream_branch: String,
}

struct StoredEntry {
    target: ProxyAddr,
    branch: String,
    upstream_branch: String,
    expires_at_ms: u64,
}

/// The TTL cache. Cheap to share behind an `Arc`.
pub struct CancelBranchLru {
    table: Mutex<HashMap<String, StoredEntry>>,
    clock: Clock,
    /// Latched by the first [`ensure_sweeper`](Self::ensure_sweeper) caller so
    /// N recv-shard cores sharing one LRU spawn exactly one sweeper.
    sweeper_claimed: AtomicBool,
}

impl CancelBranchLru {
    /// System clock.
    pub fn new() -> Self {
        Self::with_clock(Clock::system())
    }

    /// Explicit clock (tests use `Clock::test_at(..)` for deterministic
    /// eviction under `tokio::time`).
    pub fn with_clock(clock: Clock) -> Self {
        Self { table: Mutex::new(HashMap::new()), clock, sweeper_claimed: AtomicBool::new(false) }
    }

    /// Spawn the background sweeper for this LRU — once. Every `ProxyCore::run`
    /// calls this; the first caller claims it and N recv-shard cores sharing one
    /// LRU don't end up with N sweepers. `lookup` only evicts an entry looked up
    /// *after* expiry — which an answered (2xx) call never is (no CANCEL, no
    /// proxy-absorbed ACK) — so without the sweep the map (and
    /// `sip_proxy_pending_invite_lru_size`) grows ≈ the cumulative-INVITE count
    /// for the life of the process. Sweeping every half-TTL physically reclaims
    /// expired slots and re-publishes the gauge, pinning the map at ~1× working
    /// set. The task is detached (process-lifetime, like the LRU itself);
    /// supervision exits the process if a core dies, so an orphaned sweeper
    /// cannot outlive the data path in production.
    pub fn ensure_sweeper(self: &Arc<Self>, metrics: Arc<ProxyMetrics>) {
        if self.sweeper_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let lru = self.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(DEFAULT_SWEEP_INTERVAL_MS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                lru.sweep_expired();
                metrics.set_pending_invite_lru_size(lru.size() as u64);
            }
        });
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Remember the downstream target + outbound branch used on a forward.
    /// TTL is per entry: [`INVITE_ENTRY_TTL_MS`] for CANCEL/ACK correlation,
    /// [`RTX_ENTRY_TTL_MS`] for retransmission branch memos.
    pub fn remember(&self, key: &str, entry: CancelEntry, ttl_ms: u64) {
        let expires_at_ms = self.now_ms() + ttl_ms;
        self.table.lock().unwrap().insert(
            key.to_string(),
            StoredEntry {
                target: entry.target,
                branch: entry.branch,
                upstream_branch: entry.upstream_branch,
                expires_at_ms,
            },
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
            Some(e) => Some(CancelEntry {
                target: e.target.clone(),
                branch: e.branch.clone(),
                upstream_branch: e.upstream_branch.clone(),
            }),
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
        CancelEntry {
            target: ProxyAddr::new("10.0.0.2", 5070),
            branch: branch.to_string(),
            upstream_branch: String::new(),
        }
    }

    #[test]
    fn key_format_is_callid_tag_cseq() {
        assert_eq!(call_id_cseq_key("abc@h", Some("t1"), 7), "abc@h|t1|7");
        assert_eq!(call_id_cseq_key("abc@h", None, 7), "abc@h||7");
    }

    #[test]
    fn from_tag_disambiguates_the_two_dialog_directions() {
        // Both directions share the Call-ID; CSeq spaces are independent and
        // can collide on the same number — the From-tag keeps them apart.
        assert_ne!(call_id_cseq_key("c1", Some("uac"), 5), call_id_cseq_key("c1", Some("b2bua"), 5));
    }

    #[test]
    fn remember_then_lookup_returns_entry() {
        let lru = CancelBranchLru::with_clock(Clock::test_at(0));
        let k = call_id_cseq_key("call-1", Some("t"), 1);
        lru.remember(&k, entry("z9hG4bK-1"), 1000);
        assert_eq!(lru.lookup(&k).unwrap().branch, "z9hG4bK-1");
        assert_eq!(lru.size(), 1);
        assert!(lru.lookup("absent").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_their_own_ttl() {
        let lru = CancelBranchLru::with_clock(Clock::test_at(0));
        let short = call_id_cseq_key("call-1", Some("t"), 1);
        let long = call_id_cseq_key("call-2", Some("t"), 1);
        lru.remember(&short, entry("a"), 1000);
        lru.remember(&long, entry("b"), 5000);
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        assert!(lru.lookup(&short).is_none(), "short-TTL entry should have expired");
        assert!(lru.lookup(&long).is_some(), "long-TTL entry must outlive the short one");
        assert_eq!(lru.sweep_expired(), 0, "lazy lookup already evicted the expired one");
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_drops_expired() {
        let lru = CancelBranchLru::with_clock(Clock::test_at(0));
        lru.remember(&call_id_cseq_key("c1", Some("t"), 1), entry("a"), 1000);
        lru.remember(&call_id_cseq_key("c2", Some("t"), 1), entry("b"), 1000);
        tokio::time::advance(std::time::Duration::from_millis(1001)).await;
        assert_eq!(lru.sweep_expired(), 2);
        assert_eq!(lru.size(), 0);
    }
}
