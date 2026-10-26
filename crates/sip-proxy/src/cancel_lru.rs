//! [`CancelBranchLru`] — proxy-local `INVITE transaction → {target, branches,
//! cookie}` cache with per-entry TTL (port of `CancelBranchLru.ts`).
//!
//! RFC 3261 §16.10 / §17.2.3: a stateless proxy forwards a CANCEL to the same
//! downstream the matching INVITE went to. The outbound branch the downstream
//! transaction layer correlates them by is a function of the message (§16.11,
//! `crate::branch`), so what this cache carries is the TARGET.
//! An entry is keyed on the INVITE transaction as its upstream opened it
//! ([`invite_txn_key`]): the received top-Via branch and sent-by, the pair a
//! CANCEL repeats (§9.1) and §17.2.3 matches on. The dialog identity is no key
//! on its own: a spiral (§16.3) crosses this proxy twice under one Call-ID,
//! From tag and CSeq, once each way, and each pass's CANCEL belongs with its
//! own INVITE. Keying on the RECEIVED transaction rather than on the proxy's
//! outbound branch survives the LoadBalancer re-sharding a fallback selection
//! to a different worker.
//!
//! The same cache also drives the non-2xx ACK hop decision (`ackhop|` keys —
//! relay the upstream's §17.1.1.3 ACK for a relayed final to the node the
//! final arrived from; see `core/request` and `core/response.rs`) and the
//! retransmission target memo (`rtx|`-prefixed keys, see `core/request`).
//! The ACK to a final the proxy generated itself is recognised by its To-tag
//! and writes nothing here (`crate::refusals`).
//!
//! Keys are written by whoever sends the proxy a request, so the memo is
//! bounded in entries and accounted bytes ([`MemoBounds`]). Each [`Lifetime`]
//! has its own pool and share of the bounds; past a bound a pool forgets its
//! oldest writes and counts them. Expiry and eviction walk a pool's write
//! order from its front, so their work is what they remove, never the table.
//! Reads and writes take one stripe's lock and never block on I/O.

mod bounds;
mod footprint;
mod pool;
mod stripe;

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use sip_clock::Clock;
use sip_message::header::Via;
use sip_txn::timers::{INVITE_INITIAL_TIMEOUT, TIMER_F, TIMER_H};

use crate::addr::ProxyAddr;
use crate::observability::{catalogue, ProxyMetrics};
use crate::poison::{locked, INVITE_BRANCH_LRU};
use crate::strategy::RouteParams;
pub use bounds::{
    calls_per_second, MemoBounds, DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, DEFAULT_STRIPES,
    ENTRY_MS_PER_CALL, TYPICAL_ENTRY_BYTES,
};
use pool::Change;
use stripe::Stripe;

/// TTL for pending-INVITE entries (CANCEL forwarding + non-2xx ACK synthesis).
/// Must cover the downstream UA's **whole INVITE transaction window**: the
/// B2BUA answers or gives up within `sip-txn`'s `INVITE_INITIAL_TIMEOUT`
/// (158 s, which wraps its 150 s `SetupTimeout` ledger), plus the
/// final-response retransmit tail (Timer H). Imported from `sip-txn` so the
/// proxy's memory and the B2BUA's transaction timers cannot drift apart: a TTL
/// under the ringing window loses the CANCEL's target and the non-2xx ACK's
/// hop while the callee is still legally ringing.
/// Covers the DEFAULT b2bua bound; a deployment raising
/// `B2BUA_INVITE_TXN_TIMEOUT_SEC` beyond this TTL degrades late-CANCEL /
/// non-2xx-ACK hop memory.
pub const INVITE_ENTRY_TTL_MS: u64 = INVITE_INITIAL_TIMEOUT + TIMER_H;

/// TTL for retransmission target memos (`rtx|` keys): upstream retransmits
/// stop at Timer B/F (64×T1 = 32 s), so these need live no longer. They are
/// written for EVERY forwarded request — including each keepalive OPTIONS — so
/// keeping them short keeps the map at ≈ one transaction window of traffic.
pub const RTX_ENTRY_TTL_MS: u64 = TIMER_F;

/// How long a memo entry lives, and the pool of each stripe it is held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// [`INVITE_ENTRY_TTL_MS`]: an INVITE transaction's CANCEL target.
    InviteTransaction,
    /// [`RTX_ENTRY_TTL_MS`]: a retransmission target or a relayed non-2xx
    /// final's ACK hop.
    Retransmission,
}

impl Lifetime {
    pub const fn ttl_ms(self) -> u64 {
        match self {
            Lifetime::InviteTransaction => INVITE_ENTRY_TTL_MS,
            Lifetime::Retransmission => RTX_ENTRY_TTL_MS,
        }
    }
}

/// Default sweep cadence — half the SHORT (rtx) TTL, so the dominant entry
/// class is physically reclaimed near its expiry and the map stays at ~1×
/// working set.
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 16_000;

#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(DEFAULT_SWEEP_INTERVAL_MS <= RTX_ENTRY_TTL_MS);
#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(RTX_ENTRY_TTL_MS <= INVITE_ENTRY_TTL_MS);

/// The key of the INVITE transaction `via` opened upstream (§17.2.3): its
/// branch and sent-by (in `SentByRef::write_canonical`'s form), with the
/// Call-ID, From tag and CSeq number, which the INVITE, its CANCEL, its non-2xx
/// ACK and every response to them repeat. The three dialog fields keep apart
/// two transactions on one branch token (an upstream that spends a token twice
/// across a restart, or a pre-RFC-3261 one that sends none). `|` is illegal in
/// every component, so the join is unambiguous; a missing part keys as empty.
pub fn invite_txn_key(via: &Via, call_id: &str, from_tag: Option<&str>, cseq_num: u32) -> String {
    txn_key(&["inv"], via, call_id, from_tag, cseq_num)
}

/// Namespaced key for the retransmission target memo of the `method` request
/// `via` sent: [`invite_txn_key`]'s fields under `rtx|{method}|`. A genuine
/// retransmission repeats them all (§17.2.3); another request on a reused
/// branch token (a UA whose `IdGen` reset on a restart) differs in one.
pub fn retransmit_key(
    via: &Via,
    call_id: &str,
    from_tag: Option<&str>,
    method: &str,
    cseq_num: u32,
) -> String {
    txn_key(&["rtx", method], via, call_id, from_tag, cseq_num)
}

fn txn_key(
    namespace: &[&str],
    via: &Via,
    call_id: &str,
    from_tag: Option<&str>,
    cseq: u32,
) -> String {
    let branch = via.branch().unwrap_or_default();
    let sent_by = via.sent_by_ref();
    let from_tag = from_tag.unwrap_or_default();
    let ns_len: usize = namespace.iter().map(|n| n.len() + 1).sum();
    let mut key = String::with_capacity(
        ns_len + branch.len() + sent_by.host().len() + call_id.len() + from_tag.len() + 24,
    );
    for part in namespace {
        key.push_str(part);
        key.push('|');
    }
    key.push_str(branch);
    key.push('|');
    let _ = sent_by.write_canonical(&mut key);
    let _ = write!(key, "|{call_id}|{from_tag}|{cseq}");
    key
}

/// Namespaced key for the non-2xx ACK hop memo of the INVITE transaction
/// `via` opened upstream, consulted on the request path when the upstream's
/// §17.1.1.3 ACK (the INVITE's own top Via) arrives. Written in two flavours:
///  • RESPONSE path, on relaying a non-2xx INVITE final upstream — carries
///    the node the final came from, so the ACK is RELAYED to the transaction
///    that sent the final (it matches the ACK and stops retransmitting);
///  • request path `reply()`, on a final the proxy generated ITSELF — empty
///    `branch`, so the ACK is ABSORBED here (the proxy is the UAS; no
///    downstream exists).
/// `ackhop|` keeps it disjoint from the `inv|` INVITE keys and the `rtx|`
/// memos sharing this store.
pub fn ack_hop_key(via: &Via, call_id: &str, from_tag: Option<&str>, cseq_num: u32) -> String {
    txn_key(&["ackhop"], via, call_id, from_tag, cseq_num)
}

/// What we cache per remembered INVITE: the downstream target and the branch
/// we stamped on our outgoing Via (§16.11's function of the message — empty
/// marks a final the proxy generated itself, whose ACK has no downstream to
/// reach). The upstream's branch is part of the key.
///
/// `stickiness` is the cookie the INVITE's dialog rides on (the params of the
/// Record-Route this proxy minted for it, or of the Route it carried), kept only
/// when `target` is one of our workers: a CANCEL whose remembered worker has
/// died re-resolves through the same `decode_stickiness` ladder every in-dialog
/// request takes, so it reaches the node holding the call's replica. `None`
/// for a downstream target and for the retransmission memos.
///
/// `minted` is the cookie this proxy minted for the forwarded request's
/// Record-Route, and `via_cookie` the cookie it put on its own Via
/// (`core/request/via_cookie`): a retransmission repeats both, and the CANCEL
/// and non-2xx ACK of an INVITE carry its `via_cookie`, whatever the worker
/// set did since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEntry {
    pub target: ProxyAddr,
    pub branch: String,
    pub stickiness: Option<RouteParams>,
    pub minted: Option<RouteParams>,
    pub via_cookie: Option<RouteParams>,
}

/// The CANCEL / ACK / retransmission memo: [`DEFAULT_STRIPES`] independently
/// locked stripes, each bounded by its share of [`MemoBounds`]. Cheap to
/// share behind an `Arc`.
///
/// Concurrency: a key hashes to one stripe and every operation on it takes
/// that stripe's lock alone, for work proportional to what it removes; the
/// totals are lock-free counters.
pub struct CancelBranchLru {
    stripes: Box<[Mutex<Stripe>]>,
    hasher: RandomState,
    clock: Clock,
    len: AtomicUsize,
    bytes: AtomicUsize,
    evicted: AtomicU64,
    /// Latched by the first [`ensure_sweeper`](Self::ensure_sweeper) caller so
    /// N recv-shard cores sharing one LRU spawn exactly one sweeper.
    sweeper_claimed: AtomicBool,
}

/// What one expiry pass did: the entries it removed and the expiry-index
/// entries it looked at (each removed one, plus the live one that ends the
/// pass in each non-empty stripe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub removed: usize,
    pub inspected: usize,
}

impl CancelBranchLru {
    /// System clock, default bounds.
    pub fn new() -> Self {
        Self::with_clock(Clock::system())
    }

    /// Explicit clock (tests use `Clock::test_at(..)` for deterministic
    /// eviction under `tokio::time`), default bounds.
    pub fn with_clock(clock: Clock) -> Self {
        Self::with_bounds(clock, MemoBounds::default())
    }

    /// Explicit clock and bounds.
    pub fn with_bounds(clock: Clock, bounds: MemoBounds) -> Self {
        let stripes = (0..bounds.stripe_count()).map(|_| Mutex::new(Stripe::new(&bounds)));
        Self {
            stripes: stripes.collect(),
            hasher: RandomState::new(),
            clock,
            len: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            evicted: AtomicU64::new(0),
            sweeper_claimed: AtomicBool::new(false),
        }
    }

    /// Spawn the background sweeper for this LRU — once. Every `ProxyCore::run`
    /// calls this; the first caller claims it and N recv-shard cores sharing one
    /// LRU don't end up with N sweepers. A write expires its own stripe and a
    /// lookup an expired entry it meets, so a stripe nobody writes to keeps its
    /// expired entries until this sweep reclaims them; each sweep also
    /// re-publishes the memo's series ([`publish`](Self::publish)). The task is
    /// detached (process-lifetime, like the LRU itself) and restarted when a
    /// sweep panics; supervision exits the process if a core dies, so an
    /// orphaned sweeper cannot outlive the data path in production.
    pub fn ensure_sweeper(self: &Arc<Self>, metrics: Arc<ProxyMetrics>) {
        if self.sweeper_claimed.swap(true, Ordering::SeqCst) {
            return;
        }
        let lru = self.clone();
        crate::supervise::spawn(crate::supervise::BRANCH_MEMO_SWEEP, move || {
            let (lru, metrics) = (lru.clone(), metrics.clone());
            async move {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "DEFAULT_SWEEP_INTERVAL_MS is not zero"
                )]
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(
                    DEFAULT_SWEEP_INTERVAL_MS,
                ));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    lru.sweep_expired();
                    lru.publish(&metrics);
                }
            }
        });
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    fn stripe(&self, key: &str) -> MutexGuard<'_, Stripe> {
        let count = u64::try_from(self.stripes.len()).unwrap_or(1).max(1);
        let slot = usize::try_from(self.hasher.hash_one(key) % count).unwrap_or(0);
        #[expect(clippy::indexing_slicing, reason = "a remainder of the stripe count")]
        let stripe = &self.stripes[slot];
        locked(stripe, INVITE_BRANCH_LRU)
    }

    /// Fold one stripe operation's [`Change`] into the totals, under that
    /// stripe's lock: an entry's removal is then counted after its addition,
    /// and no total reads below what the stripes hold.
    fn account(&self, change: &Change) {
        self.len.fetch_add(change.added, Ordering::Relaxed);
        self.len.fetch_sub(change.removed, Ordering::Relaxed);
        self.bytes.fetch_add(change.bytes_added, Ordering::Relaxed);
        self.bytes.fetch_sub(change.bytes_removed, Ordering::Relaxed);
        let evicted = u64::try_from(change.evicted).unwrap_or(u64::MAX);
        self.evicted.fetch_add(evicted, Ordering::Relaxed);
    }

    /// Remember the downstream target + outbound branch used on a forward,
    /// for `lifetime`. A rewrite of a held key replaces its entry and its
    /// expiry. The write first forgets the expired entries of its pool, then,
    /// past a bound, that pool's oldest writes (counted in
    /// [`evicted`](Self::evicted)). Nothing is refused for want of room: a
    /// request whose memo was forgotten is routed as one this hop never saw.
    pub fn remember(&self, key: &str, entry: CancelEntry, lifetime: Lifetime) {
        let now = self.now_ms();
        let mut change = Change::default();
        let mut stripe = self.stripe(key);
        stripe.insert(key, entry, lifetime, now, &mut change);
        self.account(&change);
    }

    /// Look up a remembered entry (for a CANCEL / ACK). Lazily evicts an expired
    /// entry and returns `None` for it.
    pub fn lookup(&self, key: &str) -> Option<CancelEntry> {
        let now = self.now_ms();
        let mut change = Change::default();
        let mut stripe = self.stripe(key);
        let found = stripe.get(key, now, &mut change);
        self.account(&change);
        found
    }

    /// The entries held.
    pub fn size(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// The bytes the held entries account for.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The entries forgotten before their expiry, the memo being at a bound.
    pub fn evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    /// Drop every expired entry, stripe by stripe. The owner task calls this
    /// periodically.
    pub fn sweep_expired(&self) -> Swept {
        let now = self.now_ms();
        let mut swept = Swept { removed: 0, inspected: 0 };
        for stripe in &self.stripes {
            let mut change = Change::default();
            let mut stripe = locked(stripe, INVITE_BRANCH_LRU);
            stripe.expire(now, &mut change);
            self.account(&change);
            drop(stripe);
            swept.removed = swept.removed.saturating_add(change.removed);
            swept.inspected = swept.inspected.saturating_add(change.inspected);
        }
        swept
    }

    /// Publish the memo's size to `metrics`.
    pub fn publish(&self, metrics: &ProxyMetrics) {
        metrics.set_pending_invite_lru_size(u64::try_from(self.size()).unwrap_or(u64::MAX));
    }

    /// The memo's families ([`catalogue::BRANCH_MEMO`]) as Prometheus text,
    /// read at the scrape from the memo's own counters.
    pub fn prometheus_text(&self) -> String {
        let mut text = String::new();
        let bytes = u64::try_from(self.bytes()).unwrap_or(u64::MAX);
        catalogue::BRANCH_MEMO_BYTES.render_value(&mut text, bytes);
        catalogue::BRANCH_MEMO_EVICTIONS.render_value(&mut text, self.evicted());
        text
    }
}

impl Default for CancelBranchLru {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
