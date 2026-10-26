//! The per-peer compacted **changelog** — split into **per-partition sub-logs**
//! (ADR-0014 §Stream topology). Each peer ordinal owns two compacted ref-logs,
//! one per [`Partition`] (`Pri` = Reclaim flow, `Bak` = Backup flow), so a
//! single-flow puller drains **only its partition** with no cross-partition
//! filtering and its own watermark cursor.
//!
//! ## Shape
//! - A node-global monotonic [`counter`](Changelog) under an injected incarnation
//!   `gen`; together they are the [`Watermark`].
//! - Per peer ordinal: two [`SubLog`]s. Each maps `counter → callRef` (ascending
//!   drain order) plus a `callRef → RefState` map (latest counter + op + when it
//!   was written + the deleter's answered statement). A re-update **moves** the
//!   ref to a new counter (the old counter is removed), so a sub-log never grows
//!   past its live set. A `Delete` stays until the peer reports applying it
//!   (ADR-0055 §3), or until the whole peer log is reaped as dead.
//!
//! ## A Delete states the answer (ADR-0031 D3)
//! Compaction folds a ref's last `Put` into the `Delete` that follows it within
//! one drain, so the peer may never see the deleting node's final body. The
//! tombstone therefore keeps whether the deleting node's own copy had answered
//! the caller, and every `Delete` frame drained from it — tail or cold — carries
//! that statement. A later `Delete` stating no answer never clears one an
//! earlier `Delete` stated; a `Put` of the ref (a new call reusing it) does.
//!
//! ## Send model (ADR-0055 §3)
//! Every write moves its sub-log's head, which wakes the serve task streaming
//! that sub-log ([`ServeGuard::written`](serving::ServeGuard::written)): the
//! server drains on write, bounded by a `limit` so a huge backlog streams in
//! batches. The `serving` count keeps
//! a peer log reap-immune while a serve task is active.
//!
//! ## Flow progress (ADR-0031 D2)
//! Each sub-log also carries the position its puller last reported applying,
//! when, and whether a flow is connected at all, so a draining worker can ask
//! [`flow_caught_up`](Changelog::flow_caught_up) whether a peer holds everything
//! it ever logged for that peer ([`progress`]). The highest position a flow
//! reported also releases the turn's held messages and reaps the deletes it
//! covers; the backlog above it bounds new calls' backups ([`backlog`]).
//!
//! ## Lock discipline (ADR-0011 X8)
//! [`bump_put`](Changelog::bump_put) / [`bump_delete`](Changelog::bump_delete) take the lock *briefly* (move ref + bump counter).
//! [`drain_since`](Changelog::drain_since) collects the due callRefs under a brief
//! lock, **drops the guard**, then reads each live body from the store. Neither
//! holds the changelog lock *or* the call-DB lock across a body read/await.
//!
//! Bodies are read **live at send time**: the log stores only references; the
//! encoded body is pulled from the store when the frame is built, so a slow
//! puller always reads latest-per-call.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use repl_net::frame::{Op, Partition, Watermark};
use sip_clock::Clock;
use tokio::sync::watch;

use crate::store::{locked, REPLICA_CHANGELOG};

pub use backlog::BacklogMarks;
pub use progress::DEFAULT_CLAIM_TTL;
pub use source::{BodySource, RefMeta};

/// How long a peer's whole changelog is kept after its last activity before an
/// idle [`reap`](Changelog::reap) drops it (the peer re-bootstraps on return).
pub const DEFAULT_DEAD_PEER_TTL_MS: i64 = 300_000;

/// Per-callRef position in a compacted sub-log.
#[derive(Clone, Debug)]
struct RefState {
    counter: u64,
    op: Op,
    /// `now_ms` of the write the entry holds: the age of a pending write.
    written_ms: i64,
    /// A `Delete` of this ref stated that the deleting node had answered the
    /// call. Kept across later `Delete`s of the ref; a `Put` clears it.
    answered: bool,
    /// The call incarnation a `Delete` removed; `None` on a `Put` (its body's
    /// metadata names it at drain time).
    incarnation: Option<String>,
}

/// One drainable entry, snapshotted under the changelog lock.
struct Due {
    counter: u64,
    call_ref: String,
    op: Op,
    answered: bool,
    incarnation: Option<String>,
}

/// One compacted ref-log for a single `(peer, partition)`.
struct SubLog {
    /// `counter → callRef`, ascending — the drain order. Compacted: exactly one
    /// counter per live callRef.
    entries: BTreeMap<u64, String>,
    /// `callRef → RefState` — the latest counter/op for the ref.
    by_ref: HashMap<String, RefState>,
    /// Highest counter DROPPED by tombstone reaping. A delete is reaped only
    /// once the flow reported applying it, so a warm puller resuming below this
    /// stands for a log it never pulled and must re-bootstrap
    /// ([`needs_reset`](Changelog::needs_reset)).
    retained_floor: u64,
    /// Highest counter ever assigned in this sub-log. Unlike `entries`, it
    /// survives compaction and reaping — it is the position a caught-up puller
    /// must have reported (ADR-0031 D2), not a live-set bound.
    head: u64,
    /// The position the flow's puller last reported applying
    /// ([`Frame::Position`](repl_net::frame::Frame::Position)). `None` while
    /// no flow is connected or none has reported yet: a disconnected flow's
    /// position is unknown, never stale.
    applied: Option<progress::Claim>,
    /// Serve tasks currently streaming this sub-log. `0` ⇒ no flow carries this
    /// partition to the peer, so nothing it holds can be current.
    streams: usize,
    /// The worker identity that opened the latest flow: only its reports
    /// count ([`progress`]).
    identity: Option<String>,
    /// The highest position the peer ever reported applying on this sub-log
    /// under this node's incarnation, connected or not ([`progress`]).
    acked: watch::Sender<Watermark>,
    /// The sub-log's head counter, moved by every write: the serve task waits
    /// on it.
    written: watch::Sender<u64>,
}

impl SubLog {
    fn new(retained_floor: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            by_ref: HashMap::new(),
            retained_floor,
            head: 0,
            applied: None,
            streams: 0,
            identity: None,
            acked: watch::Sender::new(Watermark::new(0, 0)),
            written: watch::Sender::new(0),
        }
    }

    /// Move `call_ref` to `counter` with `op`, written at `now_ms`, dropping any
    /// prior slot, and wake the serve task. A `Delete` over a `Delete` keeps the
    /// answer the first one stated; a `Put` starts the ref afresh, so a reused
    /// ref never inherits a former call's.
    fn put(&mut self, counter: u64, call_ref: &str, op: Op, now_ms: i64, stated: Stated) {
        let Stated { mut answered, incarnation } = stated;
        if let Some(prev) = self.by_ref.get(call_ref) {
            self.entries.remove(&prev.counter);
            answered |= op == Op::Delete && prev.op == Op::Delete && prev.answered;
        }
        self.entries.insert(counter, call_ref.to_string());
        let state = RefState { counter, op, written_ms: now_ms, answered, incarnation };
        self.by_ref.insert(call_ref.to_string(), state);
        self.head = self.head.max(counter);
        self.written.send_replace(self.head);
    }

    /// Drop `call_ref`'s entry, whatever its op.
    fn forget(&mut self, call_ref: &str) {
        if let Some(st) = self.by_ref.remove(call_ref) {
            self.entries.remove(&st.counter);
        }
    }

    /// The due entries above `since` (or all when `cold`), ascending, capped at
    /// `limit`.
    fn due(&self, since_counter: u64, cold: bool, limit: usize) -> Vec<Due> {
        let start = if cold { Bound::Unbounded } else { Bound::Excluded(since_counter) };
        self.entries
            .range((start, Bound::Unbounded))
            .take(limit)
            .filter_map(|(c, call_ref)| {
                let st = self.by_ref.get(call_ref)?;
                Some(Due {
                    counter: *c,
                    call_ref: call_ref.clone(),
                    op: st.op,
                    answered: st.answered,
                    incarnation: st.incarnation.clone(),
                })
            })
            .collect()
    }
}

/// What a `Delete` entry states beside its op ([`Changelog::bump_delete`]).
#[derive(Default)]
struct Stated {
    answered: bool,
    incarnation: Option<String>,
}

/// A single peer's two compacted sub-logs + liveness clock + serve guard.
struct PeerLog {
    pri: SubLog,
    bak: SubLog,
    /// `now_ms` of the last mutation recorded — drives idle reaping.
    last_active_ms: i64,
    /// Count of active serve tasks for this peer. A log with `serving > 0` is
    /// NOT reaped while idle, so a server never loses the log it is actively
    /// draining out from under itself.
    serving: usize,
    /// New calls are admitted with no backup on this peer: its backlog rose
    /// over the high-water mark and has not fallen under the low one since
    /// ([`backlog`]).
    over_high_water: bool,
}

impl PeerLog {
    /// A log opened when the node's counter stood at `counter`: it holds
    /// nothing logged before, so a warm pull below it re-bootstraps.
    fn new(now_ms: i64, counter: u64) -> Self {
        Self {
            pri: SubLog::new(counter),
            bak: SubLog::new(counter),
            last_active_ms: now_ms,
            serving: 0,
            over_high_water: false,
        }
    }

    fn sub(&self, partition: Partition) -> &SubLog {
        match partition {
            Partition::Pri => &self.pri,
            Partition::Bak => &self.bak,
        }
    }

    fn sub_mut(&mut self, partition: Partition) -> &mut SubLog {
        match partition {
            Partition::Pri => &mut self.pri,
            Partition::Bak => &mut self.bak,
        }
    }
}

struct Inner {
    counter: u64,
    peers: HashMap<String, PeerLog>,
    /// How long a flow's claim vouches for it ([`progress`]).
    claim_ttl: std::time::Duration,
    /// When the backlog toward a peer stops and resumes giving new calls a
    /// backup ([`backlog`]).
    marks: BacklogMarks,
}

/// Node-global monotonic changelog over per-peer, per-partition compacted
/// ref-logs. Clone-cheap (one `Arc`); share across the call path and the server.
#[derive(Clone)]
pub struct Changelog {
    gen: u64,
    clock: Clock,
    dead_peer_ttl_ms: i64,
    inner: Arc<Mutex<Inner>>,
}

impl Changelog {
    /// Build a changelog under incarnation `gen` with `clock` for TTL math.
    pub fn new(gen: u64, clock: Clock) -> Self {
        Self {
            gen,
            clock,
            dead_peer_ttl_ms: DEFAULT_DEAD_PEER_TTL_MS,
            inner: Arc::new(Mutex::new(Inner {
                counter: 0,
                peers: HashMap::new(),
                claim_ttl: DEFAULT_CLAIM_TTL,
                marks: BacklogMarks::default(),
            })),
        }
    }

    /// Poison the changelog's lock, as a panic under it would.
    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let inner = self.inner.clone();
        let _ = std::thread::spawn(move || {
            let _held = inner.lock();
            panic!("poison the changelog lock");
        })
        .join();
        assert!(self.inner.is_poisoned());
    }

    /// Override the dead-peer TTL (tests use short values).
    pub fn with_dead_peer_ttl(mut self, dead_peer_ttl_ms: i64) -> Self {
        self.dead_peer_ttl_ms = dead_peer_ttl_ms;
        self
    }

    /// The current head `(gen, counter)`.
    pub fn head(&self) -> Watermark {
        Watermark::new(self.gen, locked(&self.inner, REPLICA_CHANGELOG).counter)
    }

    /// This node's shared wall clock (`Clock::now_ms()`), the send-time reading
    /// a bootstrap `Data` frame's `origin_now_ms` is derived from
    /// ([`RefMeta::origin_now_ms`]). Uses the SAME clock the changelog TTL math
    /// and `drain_since` stamp read.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Whether a warm puller on `partition` resuming from `since` must
    /// re-bootstrap. True when:
    /// - `since.gen > self.gen` — a watermark from a FUTURE incarnation (the
    ///   wall clock stepped backward across our restart, or a gen collision):
    ///   meaningless against this log, and the warm path would silently skip
    ///   everything until our counter outgrew it;
    /// - `since.gen == self.gen` and `since.counter` exceeds our head — a
    ///   counter we never issued, i.e. a same-instant gen collision after a
    ///   fast restart (the counter restarted at 0 under a reused gen);
    /// - `since.gen == self.gen` and `since.counter` is below the partition's
    ///   `retained_floor` — fell off the compacted tail (a reaped tombstone the
    ///   puller never pulled).
    ///
    /// `since.gen < self.gen` is the ordinary cold case — `drain_since` already
    /// treats it as drain-everything, no reset needed.
    pub fn needs_reset(&self, peer: &str, partition: Partition, since: Watermark) -> bool {
        if since.gen > self.gen {
            return true;
        }
        if since.gen < self.gen {
            return false;
        }
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        if since.counter > inner.counter {
            return true;
        }
        inner
            .peers
            .get(peer)
            .map(|log| since.counter < log.sub(partition).retained_floor)
            .unwrap_or(false)
    }

    /// Record a `Put` of `call_ref` for `(peer, partition)` (Create/Update
    /// merged — ADR-0014). Its body states whether the call was answered, so the
    /// entry states nothing: a `Put` starts the ref afresh.
    pub fn bump_put(&self, peer: &str, call_ref: &str, partition: Partition) -> Watermark {
        self.bump(peer, call_ref, Op::Put, partition, Stated::default())
    }

    /// Record a `Delete` of `call_ref` for `(peer, partition)`, with the deleting
    /// node's statement that its copy had answered the call. The tombstone keeps
    /// the statement; a `Delete` following a `Delete` keeps an answer the first
    /// one stated (ADR-0031 D3). The `Delete` names the call incarnation it
    /// removed, so its receiver buries that call and no other.
    pub fn bump_delete(
        &self,
        peer: &str,
        call_ref: &str,
        partition: Partition,
        answered: bool,
        incarnation: Option<String>,
    ) -> Watermark {
        self.bump(peer, call_ref, Op::Delete, partition, Stated { answered, incarnation })
    }

    /// Record a mutation for `(peer, partition)`: assign the next counter,
    /// **compact** (drop the ref's previous counter in that sub-log), set its
    /// op, and wake the serve task streaming the sub-log. Returns the entry's
    /// position.
    fn bump(
        &self,
        peer: &str,
        call_ref: &str,
        op: Op,
        partition: Partition,
        stated: Stated,
    ) -> Watermark {
        let now = self.clock.now_ms();
        let inner = &mut *locked(&self.inner, REPLICA_CHANGELOG);
        inner.counter += 1;
        let c = inner.counter;
        let counter = c - 1;
        let log = inner.peers.entry(peer.to_string()).or_insert_with(|| PeerLog::new(now, counter));
        log.last_active_ms = now;
        log.sub_mut(partition).put(c, call_ref, op, now, stated);
        Watermark::new(self.gen, c)
    }

    /// Drop `call_ref`'s entry toward `peer` on `partition`: the peer refused
    /// its replica (ADR-0055 §3), and no later write of the call goes to it.
    pub fn forget(&self, peer: &str, partition: Partition, call_ref: &str) {
        let mut inner = locked(&self.inner, REPLICA_CHANGELOG);
        if let Some(log) = inner.peers.get_mut(peer) {
            log.sub_mut(partition).forget(call_ref);
        }
    }

    /// The position of `call_ref`'s entry toward `peer` on `partition`, `None`
    /// when it has none.
    pub fn position_of(
        &self,
        peer: &str,
        partition: Partition,
        call_ref: &str,
    ) -> Option<Watermark> {
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        let st = inner.peers.get(peer)?.sub(partition).by_ref.get(call_ref)?;
        Some(Watermark::new(self.gen, st.counter))
    }

    /// `(entries, peers)` outbound-buffer depth for the memory-attribution
    /// gauges: total compacted entries summed across every peer's two sub-logs,
    /// and the live peer-log count. A peer whose entries grow without draining is
    /// an outbound leak distinct from the call map. One brief lock; pure read.
    pub fn depth(&self) -> (u64, u64) {
        let inner = locked(&self.inner, REPLICA_CHANGELOG);
        let entries: usize =
            inner.peers.values().map(|p| p.pri.entries.len() + p.bak.entries.len()).sum();
        (entries as u64, inner.peers.len() as u64)
    }

    /// Evict the tombstones each peer reported applying, and idle peers. Call
    /// after advancing the clock (lazy TTL — deterministic, no background task,
    /// no `DelayQueue` aliasing).
    pub fn reap(&self, now_ms: i64) {
        let dead_peer_ttl = self.dead_peer_ttl_ms;
        let gen = self.gen;
        let mut inner = locked(&self.inner, REPLICA_CHANGELOG);
        // Drop idle peers wholesale — but NEVER one with an active serve task.
        inner.peers.retain(|_, log| log.serving > 0 || now_ms - log.last_active_ms < dead_peer_ttl);
        // Reap applied tombstones from each surviving sub-log, raising its floor.
        for log in inner.peers.values_mut() {
            reap_sublog(&mut log.pri, gen);
            reap_sublog(&mut log.bak, gen);
        }
    }

    /// Live entries in a peer's sub-log (test introspection).
    #[cfg(test)]
    pub fn peer_len(&self, peer: &str, partition: Partition) -> usize {
        self.inner
            .lock()
            .unwrap()
            .peers
            .get(peer)
            .map(|l| l.sub(partition).entries.len())
            .unwrap_or(0)
    }

    /// Whether a peer log exists (test introspection).
    #[cfg(test)]
    pub fn has_peer(&self, peer: &str) -> bool {
        locked(&self.inner, REPLICA_CHANGELOG).peers.contains_key(peer)
    }
}

/// Remove from `sub` the tombstones its peer reported applying under
/// incarnation `gen`, raising its `retained_floor`.
fn reap_sublog(sub: &mut SubLog, gen: u64) {
    let acked = *sub.acked.borrow();
    if acked.gen != gen {
        return;
    }
    let applied: Vec<String> = sub
        .entries
        .range(..=acked.counter)
        .filter(|(_, call_ref)| sub.by_ref.get(*call_ref).is_some_and(|st| st.op == Op::Delete))
        .map(|(_, call_ref)| call_ref.clone())
        .collect();
    for call_ref in applied {
        if let Some(st) = sub.by_ref.remove(&call_ref) {
            sub.entries.remove(&st.counter);
            sub.retained_floor = sub.retained_floor.max(st.counter);
        }
    }
}

mod backlog;
mod drain;
mod progress;
mod serving;
mod source;
#[cfg(test)]
mod tests;
