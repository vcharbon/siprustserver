//! [`CancelBranchLru`]: the proxy-local memo of `INVITE transaction →
//! {target, branches, cookie}`, with a TTL per entry.
//!
//! It holds three kinds of entry: an INVITE transaction's CANCEL target (RFC
//! 3261 §16.10), keyed on the transaction as received ([`invite_txn_key`]: the
//! top-Via branch and sent-by, §9.1, §17.2.3); the hop of the ACK to a relayed
//! non-2xx final (`ackhop|` keys, §17.1.1.3); and a request's retransmission
//! target (`rtx|` keys). The ACK to a final the proxy generated itself writes
//! nothing here (`crate::refusals`). Every request these entries route also
//! routes without them. See ADR-0009 X1, X7.
//!
//! Keys are written by whoever sends the proxy a request, so the memo is
//! bounded in entries and accounted bytes ([`MemoBounds`]). Each
//! [`Lifetime`] has its own pool and share of the bounds; past a bound a pool
//! forgets its oldest writes and counts them. Expiry and eviction walk a pool's
//! write order from its front, so their work is what they remove, never the
//! table. Reads and writes take one stripe's lock and never block on I/O.

mod bounds;
mod footprint;
mod pool;
mod stripe;

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::BuildHasher;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use sip_clock::Clock;
use sip_message::header::Via;
use sip_txn::timers::{TIMER_F, TIMER_H};

use crate::addr::ProxyAddr;
use crate::observability::{catalogue, ProxyMetrics};
use crate::poison::{locked, INVITE_BRANCH_LRU};
use crate::strategy::RouteParams;
pub use bounds::{
    calls_per_second, entry_ms_per_call, invite_entry_ttl_ms, MemoBounds, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_ENTRIES, DEFAULT_MEAN_RING_MS, DEFAULT_STRIPES, INVITE_ENTRY_TTL_MS,
    TYPICAL_ENTRY_BYTES,
};
use pool::Change;
use stripe::Stripe;

/// TTL of a retransmission target memo (`rtx|` keys): Timer F (64×T1), past
/// which no upstream retransmits. It is also Timer H, the life of an ACK hop
/// and of an INVITE entry whose final passed.
pub const RTX_ENTRY_TTL_MS: u64 = TIMER_F;

/// How long a memo entry lives, and the pool of each stripe it is held in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    /// [`MemoBounds::invite_ttl_ms`], the cap derived from the downstream
    /// INVITE transaction bound ([`invite_entry_ttl_ms`]), shortened to Timer H
    /// when the final passes ([`CancelBranchLru::shorten`]): an INVITE
    /// transaction's CANCEL target.
    InviteTransaction,
    /// [`RTX_ENTRY_TTL_MS`]: a retransmission target, a relayed non-2xx
    /// final's ACK hop, or an INVITE transaction's entry once its final
    /// passed.
    Retransmission,
}

/// Default sweep cadence: half [`RTX_ENTRY_TTL_MS`], so a retransmission
/// entry, the most numerous, is reclaimed within half its TTL of expiring.
pub const DEFAULT_SWEEP_INTERVAL_MS: u64 = 16_000;

#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(DEFAULT_SWEEP_INTERVAL_MS <= RTX_ENTRY_TTL_MS);
#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(RTX_ENTRY_TTL_MS <= INVITE_ENTRY_TTL_MS);
#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(RTX_ENTRY_TTL_MS == TIMER_H);

/// The key of the INVITE transaction `via` opened upstream (§17.2.3): its
/// branch and sent-by (in `SentByRef::write_canonical`'s form), with the
/// Call-ID, From tag and CSeq number, which the INVITE, its CANCEL, its non-2xx
/// ACK and every response to them repeat. The three dialog fields keep apart
/// two transactions on one branch token (an upstream that spends a token twice
/// across a restart). `|` is illegal in every component, so the join is
/// unambiguous; a missing part keys as empty.
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

/// The branch is folded to lower case in the key: a branch compares ignoring
/// case (RFC 3261 §7.3.1).
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
    key.extend(branch.chars().map(|c| c.to_ascii_lowercase()));
    key.push('|');
    let _ = sent_by.write_canonical(&mut key);
    let _ = write!(key, "|{call_id}|{from_tag}|{cseq}");
    key
}

/// Namespaced key for the non-2xx ACK hop memo of the INVITE transaction
/// `via` opened upstream, consulted on the request path when the upstream's
/// §17.1.1.3 ACK (the INVITE's own top Via) arrives. Written on the response
/// path for every non-2xx INVITE final this proxy relays, whether or not the
/// INVITE crossed it: it carries the node the final came from, so the ACK is
/// RELAYED to the transaction that sent the final (it matches the ACK and
/// stops retransmitting). `ackhop|` keeps it disjoint from the `inv|` INVITE
/// keys and the `rtx|` memos sharing this store.
pub fn ack_hop_key(via: &Via, call_id: &str, from_tag: Option<&str>, cseq_num: u32) -> String {
    txn_key(&["ackhop"], via, call_id, from_tag, cseq_num)
}

/// What we cache per remembered INVITE: the downstream target and the branch
/// we stamped on our outgoing Via (§16.11's function of the message — empty
/// marks a final the proxy generated itself, whose ACK has no downstream to
/// reach). The upstream's branch is part of the key. A named target becomes
/// the address its send left for once that send settles it
/// ([`CancelBranchLru::settle_name`]).
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
    /// `target` is the address a send to a name left for, written by
    /// [`CancelBranchLru::settle_name`] and kept by a copy's rewrite of the
    /// entry. The face a request leaves on is the name's, so no registry match
    /// of that address moves it.
    pub settled: bool,
}

impl CancelEntry {
    /// Whether `target` is this entry's address settled from a name.
    pub fn settled_on(&self, target: &ProxyAddr) -> bool {
        self.settled && self.target == *target
    }
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

    /// Default bounds, read on `clock`.
    pub fn with_clock(clock: Clock) -> Self {
        Self::with_bounds(clock, MemoBounds::default())
    }

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

    /// Spawn the memo's background sweeper, once whatever the number of
    /// callers. A write expires only its own stripe and a lookup only the entry
    /// it meets, so the sweep reclaims the stripes nobody writes to; each sweep
    /// also republishes the memo's series ([`publish`](Self::publish)). The
    /// task lives as long as the process and restarts when a sweep panics.
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

    /// The live entry under `key`, an INVITE transaction's whose final has
    /// passed: from now on it lives [`Lifetime::Retransmission`] (Timer H, the
    /// window of the ACK to a non-2xx final, RFC 3261 §17.2.1) instead of
    /// its INVITE cap. An entry already shortened keeps its expiry.
    pub fn shorten(&self, key: &str) -> Option<CancelEntry> {
        let now = self.now_ms();
        let mut change = Change::default();
        let mut stripe = self.stripe(key);
        let found = stripe.shorten(key, now, &mut change);
        self.account(&change);
        found
    }

    /// The entry under `key`, while it targets the name `name`, targets `to`,
    /// the address a send to that name left for: the requests it routes
    /// (a CANCEL, a non-2xx ACK, a retransmission) go there without a lookup
    /// (RFC 3261 §9.1, §17.1.1.3). Its expiry stays.
    pub fn settle_name(&self, key: &str, name: &ProxyAddr, to: SocketAddr) {
        let mut change = Change::default();
        let mut stripe = self.stripe(key);
        stripe.retarget(key, name, &ProxyAddr::from(to), &mut change);
        self.account(&change);
    }

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
