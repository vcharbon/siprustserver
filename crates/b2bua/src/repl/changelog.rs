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
//!   drain order) plus a `callRef → RefState` map (latest counter + op + optional
//!   tombstone expiry). A re-update **moves** the ref to a new counter (the old
//!   counter is removed), so a sub-log never grows past its live set.
//!
//! ## Send model (ADR-0014)
//! There is **no `Notify`/subscription**: the server is a **pure poll loop** that
//! calls [`drain_since`](Changelog::drain_since) every ~100ms (bounded by a
//! `limit` so a huge backlog streams in batches). The `serving` count keeps a
//! peer log reap-immune while a serve task is active.
//!
//! ## Lock discipline (ADR-0011 X8)
//! [`bump`](Changelog::bump) takes the lock *briefly* (move ref + bump counter).
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

use repl_net::frame::{Frame, Op, Partition, Watermark};
use sip_clock::Clock;

use crate::store::PartitionRole;

/// How long a delete tombstone is retained so a disconnected puller still learns
/// of the removal before the ref is reaped.
pub const DEFAULT_TOMBSTONE_TTL_MS: i64 = 30_000;

/// How long a peer's whole changelog is kept after its last activity before an
/// idle [`reap`](Changelog::reap) drops it (the peer re-bootstraps on return).
pub const DEFAULT_DEAD_PEER_TTL_MS: i64 = 300_000;

/// Per-callRef metadata needed to build a [`Frame::Data`] without re-reading the
/// (typed) call. Carried alongside the body by [`ReplicatingCallStore`].
///
/// [`ReplicatingCallStore`]: super::store::ReplicatingCallStore
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefMeta {
    /// Primary counter `p` of the `(p,b)` version vector.
    pub call_gen: i64,
    /// Backup counter `b` of the `(p,b)` version vector (ADR-0014).
    pub call_bgen: i64,
    /// Body TTL in ms (the store `ttl`).
    pub body_ttl_ms: i64,
    /// Index keys for this call.
    pub indexes: Vec<String>,
}

/// The store-side seam the drain reads through to build [`Frame::Data`]. The
/// changelog stays decoupled from [`ReplicatingCallStore`]: it asks for a live
/// body (`Arc<[u8]>`) and the per-ref metadata.
///
/// [`ReplicatingCallStore`]: super::store::ReplicatingCallStore
#[async_trait::async_trait]
pub trait BodySource: Send + Sync {
    /// The live encoded body for a callRef in `(role, primary)`, or `None` if it
    /// is gone (→ the drain emits a `Delete`).
    async fn read_body(
        &self,
        role: PartitionRole,
        primary: &str,
        call_ref: &str,
    ) -> Option<Arc<[u8]>>;

    /// The per-ref metadata (call_gen / ttl / indexes), or `None` if gone.
    fn read_meta(&self, call_ref: &str) -> Option<RefMeta>;

    /// Snapshot the live callRef KEYS in `(role, primary)` under a BRIEF lock.
    /// The **Reclaim** bootstrap copies the `bak:{caller}` keyset this way.
    fn scan_refs(&self, _role: PartitionRole, _primary: &str) -> Vec<String> {
        Vec::new()
    }

    /// Snapshot the live callRef KEYS this node is **primary** for whose **backup
    /// is `backup`** (ADR-0014 Option B). The **Backup** bootstrap uses this to
    /// scan `pri:{self}` filtered to the calls `caller` backs up.
    fn scan_refs_backed_by(&self, _primary: &str, _backup: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Per-callRef position in a compacted sub-log.
#[derive(Clone, Debug)]
struct RefState {
    counter: u64,
    op: Op,
    /// Set when `op == Delete`: absolute reap deadline for the tombstone.
    expiry_at_ms: Option<i64>,
}

/// One compacted ref-log for a single `(peer, partition)`.
#[derive(Default)]
struct SubLog {
    /// `counter → callRef`, ascending — the drain order. Compacted: exactly one
    /// counter per live callRef.
    entries: BTreeMap<u64, String>,
    /// `callRef → RefState` — the latest counter/op for the ref.
    by_ref: HashMap<String, RefState>,
    /// Highest counter DROPPED by tombstone reaping (a delete the puller may have
    /// missed). A warm puller resuming below this has fallen off the compacted
    /// tail and must re-bootstrap ([`needs_reset`](Changelog::needs_reset)).
    retained_floor: u64,
}

impl SubLog {
    /// Move `call_ref` to `counter` with `op`/`expiry`, dropping any prior slot.
    fn put(&mut self, counter: u64, call_ref: &str, op: Op, expiry_at_ms: Option<i64>) {
        if let Some(prev) = self.by_ref.get(call_ref) {
            self.entries.remove(&prev.counter);
        }
        self.entries.insert(counter, call_ref.to_string());
        self.by_ref.insert(
            call_ref.to_string(),
            RefState {
                counter,
                op,
                expiry_at_ms,
            },
        );
    }

    /// The due `(counter, callRef, op)` above `since` (or all when `cold`),
    /// ascending, capped at `limit`.
    fn due(&self, since_counter: u64, cold: bool, limit: usize) -> Vec<(u64, String, Op)> {
        let start = if cold {
            Bound::Unbounded
        } else {
            Bound::Excluded(since_counter)
        };
        self.entries
            .range((start, Bound::Unbounded))
            .take(limit)
            .map(|(c, call_ref)| (*c, call_ref.clone(), self.by_ref[call_ref].op))
            .collect()
    }
}

/// A single peer's two compacted sub-logs + liveness clock + serve guard.
struct PeerLog {
    pri: SubLog,
    bak: SubLog,
    /// `now_ms` of the last [`bump`](Changelog::bump) — drives idle reaping.
    last_active_ms: i64,
    /// Count of active serve tasks for this peer. A log with `serving > 0` is
    /// NOT reaped while idle, so a poll-loop server never loses the log it is
    /// actively draining out from under itself.
    serving: usize,
}

impl PeerLog {
    fn new(now_ms: i64) -> Self {
        Self {
            pri: SubLog::default(),
            bak: SubLog::default(),
            last_active_ms: now_ms,
            serving: 0,
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

/// RAII handle keeping a peer log reap-immune while a serve task drains it.
/// Decrements the `serving` count on drop.
pub struct ServeGuard {
    changelog: Changelog,
    peer: String,
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let mut inner = self.changelog.inner.lock().unwrap();
        if let Some(log) = inner.peers.get_mut(&self.peer) {
            log.serving = log.serving.saturating_sub(1);
        }
    }
}

struct Inner {
    counter: u64,
    peers: HashMap<String, PeerLog>,
}

/// Node-global monotonic changelog over per-peer, per-partition compacted
/// ref-logs. Clone-cheap (one `Arc`); share across the call path and the server.
#[derive(Clone)]
pub struct Changelog {
    gen: u64,
    clock: Clock,
    tombstone_ttl_ms: i64,
    dead_peer_ttl_ms: i64,
    inner: Arc<Mutex<Inner>>,
}

impl Changelog {
    /// Build a changelog under incarnation `gen` with `clock` for TTL math.
    pub fn new(gen: u64, clock: Clock) -> Self {
        Self {
            gen,
            clock,
            tombstone_ttl_ms: DEFAULT_TOMBSTONE_TTL_MS,
            dead_peer_ttl_ms: DEFAULT_DEAD_PEER_TTL_MS,
            inner: Arc::new(Mutex::new(Inner {
                counter: 0,
                peers: HashMap::new(),
            })),
        }
    }

    /// Override the tombstone / dead-peer TTLs (tests use short values).
    pub fn with_ttls(mut self, tombstone_ttl_ms: i64, dead_peer_ttl_ms: i64) -> Self {
        self.tombstone_ttl_ms = tombstone_ttl_ms;
        self.dead_peer_ttl_ms = dead_peer_ttl_ms;
        self
    }

    /// The current head `(gen, counter)`.
    pub fn head(&self) -> Watermark {
        Watermark::new(self.gen, self.inner.lock().unwrap().counter)
    }

    /// Register a serve task for `peer`, keeping its log reap-immune until the
    /// returned [`ServeGuard`] drops. Creates the peer log if absent.
    pub fn serving(&self, peer: &str) -> ServeGuard {
        {
            let mut inner = self.inner.lock().unwrap();
            let now = self.clock.now_ms();
            let log = inner
                .peers
                .entry(peer.to_string())
                .or_insert_with(|| PeerLog::new(now));
            log.serving += 1;
        }
        ServeGuard {
            changelog: self.clone(),
            peer: peer.to_string(),
        }
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
    ///   puller may have missed).
    /// `since.gen < self.gen` is the ordinary cold case — `drain_since` already
    /// treats it as drain-everything, no reset needed.
    pub fn needs_reset(&self, peer: &str, partition: Partition, since: Watermark) -> bool {
        if since.gen > self.gen {
            return true;
        }
        if since.gen < self.gen {
            return false;
        }
        let inner = self.inner.lock().unwrap();
        if since.counter > inner.counter {
            return true;
        }
        inner
            .peers
            .get(peer)
            .map(|log| since.counter < log.sub(partition).retained_floor)
            .unwrap_or(false)
    }

    /// Record a mutation for `(peer, partition)`: assign the next counter,
    /// **compact** (drop the ref's previous counter in that sub-log), set its
    /// op, and stamp a tombstone expiry on `Delete`. **No notify** (pure-poll
    /// server). `op` is `Put` or `Delete` (Create/Update merged — ADR-0014).
    pub fn bump(&self, peer: &str, call_ref: &str, op: Op, partition: Partition) {
        let now = self.clock.now_ms();
        let inner = &mut *self.inner.lock().unwrap();
        inner.counter += 1;
        let c = inner.counter;

        let log = inner
            .peers
            .entry(peer.to_string())
            .or_insert_with(|| PeerLog::new(now));
        log.last_active_ms = now;

        let expiry_at_ms = match op {
            Op::Delete => Some(now + self.tombstone_ttl_ms),
            Op::Put => None,
        };
        log.sub_mut(partition).put(c, call_ref, op, expiry_at_ms);
    }

    /// Drain a `(peer, partition)` sub-log's entries with `counter > since.counter`
    /// (or **all** when `since.gen < self.gen` — the cold/bootstrap case),
    /// ascending, **capped at `limit`**, as [`Frame::Data`]. Bodies are read
    /// **live** from `source` with the changelog lock **dropped**.
    ///
    /// `role`/`primary` locate the body in the store's keyspace (fixed by the
    /// flow: `Pri` ⇒ `(Backup, caller)`, `Bak` ⇒ `(Primary, self)`). A `Delete`
    /// ref or a vanished body yields `Frame::Data{ op: Delete, body: None, .. }`.
    #[allow(clippy::too_many_arguments)]
    pub async fn drain_since(
        &self,
        peer: &str,
        partition: Partition,
        since: Watermark,
        limit: usize,
        source: &dyn BodySource,
        role: PartitionRole,
        primary: &str,
    ) -> Vec<Frame> {
        // Brief lock: snapshot the due (counter, callRef, op) tuples, then DROP
        // the guard before any body read/await.
        let due: Vec<(u64, String, Op)> = {
            let inner = self.inner.lock().unwrap();
            let Some(log) = inner.peers.get(peer) else {
                return Vec::new();
            };
            let cold = since.gen < self.gen;
            log.sub(partition).due(since.counter, cold, limit)
        };

        let mut frames = Vec::with_capacity(due.len());
        for (counter, call_ref, op) in due {
            let at = Watermark::new(self.gen, counter);
            if op == Op::Delete {
                frames.push(delete_frame(at, partition, call_ref));
                continue;
            }
            // Body read at send time — lock already dropped.
            let body = source.read_body(role, primary, &call_ref).await;
            match (body, source.read_meta(&call_ref)) {
                (Some(body), Some(meta)) => frames.push(Frame::Data {
                    at,
                    op: Op::Put,
                    partition,
                    call_ref,
                    call_gen: meta.call_gen,
                    call_bgen: meta.call_bgen,
                    body_ttl_ms: meta.body_ttl_ms,
                    indexes: meta.indexes,
                    body: Some(body),
                }),
                // Gone between snapshot and read → emit a delete.
                _ => frames.push(delete_frame(at, partition, call_ref)),
            }
        }
        frames
    }

    /// Drop a peer's whole changelog (auto-clean on disconnect; it re-bootstraps
    /// on return). Returns `true` if a log existed.
    pub fn drop_peer(&self, peer: &str) -> bool {
        self.inner.lock().unwrap().peers.remove(peer).is_some()
    }

    /// `(entries, peers)` outbound-buffer depth for the memory-attribution
    /// gauges: total compacted entries summed across every peer's two sub-logs,
    /// and the live peer-log count. A peer whose entries grow without draining is
    /// an outbound leak distinct from the call map. One brief lock; pure read.
    pub fn depth(&self) -> (u64, u64) {
        let inner = self.inner.lock().unwrap();
        let entries: usize = inner
            .peers
            .values()
            .map(|p| p.pri.entries.len() + p.bak.entries.len())
            .sum();
        (entries as u64, inner.peers.len() as u64)
    }

    /// Evict expired tombstones and idle peers. Call after advancing the clock
    /// (lazy TTL — deterministic, no background task, no `DelayQueue` aliasing).
    pub fn reap(&self, now_ms: i64) {
        let dead_peer_ttl = self.dead_peer_ttl_ms;
        let mut inner = self.inner.lock().unwrap();
        // Drop idle peers wholesale — but NEVER one with an active serve task.
        inner
            .peers
            .retain(|_, log| log.serving > 0 || now_ms - log.last_active_ms < dead_peer_ttl);
        // Reap expired tombstones from each surviving sub-log, raising its floor.
        for log in inner.peers.values_mut() {
            reap_sublog(&mut log.pri, now_ms);
            reap_sublog(&mut log.bak, now_ms);
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
        self.inner.lock().unwrap().peers.contains_key(peer)
    }
}

/// Build a `Delete` `Data` frame (nil body, zero meta) at `at`/`partition`.
fn delete_frame(at: Watermark, partition: Partition, call_ref: String) -> Frame {
    Frame::Data {
        at,
        op: Op::Delete,
        partition,
        call_ref,
        call_gen: 0,
        call_bgen: 0,
        body_ttl_ms: 0,
        indexes: Vec::new(),
        body: None,
    }
}

/// Remove expired tombstones from `sub`, raising its `retained_floor`.
fn reap_sublog(sub: &mut SubLog, now_ms: i64) {
    let expired: Vec<String> = sub
        .by_ref
        .iter()
        .filter(|(_, st)| matches!(st.expiry_at_ms, Some(e) if now_ms >= e))
        .map(|(call_ref, _)| call_ref.clone())
        .collect();
    for call_ref in expired {
        if let Some(st) = sub.by_ref.remove(&call_ref) {
            sub.entries.remove(&st.counter);
            sub.retained_floor = sub.retained_floor.max(st.counter);
        }
    }
}
