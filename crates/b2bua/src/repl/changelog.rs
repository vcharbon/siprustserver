//! The per-peer compacted **changelog** — the in-process equivalent of the
//! source's compacted `propagate:{peer}` Redis ZSET (callRef-keyed,
//! counter-scored ⇒ implicitly compacted). ADR-0011 X3 / Decision 2.
//!
//! ## Shape
//! - A node-global monotonic [`counter`](Changelog) under an injected incarnation
//!   `gen`; together they are the [`Watermark`].
//! - Per peer ordinal: a compacted log mapping `counter → callRef` (ascending
//!   drain order) plus a `callRef → RefState` map (the latest counter + op +
//!   partition + optional tombstone expiry). A re-update **moves** the ref to a
//!   new counter (the old counter is removed), so the log never grows past the
//!   live set — stale intermediates are shed for free.
//!
//! ## Lock discipline (ADR-0011 X8)
//! The mutation path ([`bump`](Changelog::bump)) takes the changelog lock
//! *briefly* (move ref + bump counter) and then notifies the peer's subscriber
//! **non-blocking** — never awaiting a subscriber, mirroring
//! `BufferedTerminateWriter`. The drain path ([`drain_since`](Changelog::drain_since))
//! collects the due callRefs under a brief lock, **drops the guard**, then reads
//! each live body from the store (`get_call` → `Arc<[u8]>` clone). Neither path
//! holds the changelog lock *or* the call-DB lock across the body read/await.
//!
//! Bodies are read **live at send time** (Decision 2): the log stores only
//! references; the encoded body is pulled from the store when the frame is
//! built, so a slow puller always reads latest-per-call.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use repl_net::frame::{Frame, Op, Partition, Watermark};
use sip_clock::Clock;
use tokio::sync::Notify;

use crate::store::PartitionRole;

/// How long a delete tombstone is retained so a disconnected puller still learns
/// of the removal before the ref is reaped. JS-faithful brief tombstone.
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
    /// Primary counter `p` of the `(p,b)` version vector (was the single LWW
    /// `call_gen`); persisted so the drained frame carries it.
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

    /// Snapshot the live callRef KEYS in `(role, primary)` under a BRIEF lock
    /// (S6 bootstrap: copy the `bak:{primary}` keyset, drop the lock, then read
    /// bodies lazily per batch). The default returns empty for sources that do
    /// not back a store (only [`ReplicatingCallStore`] overrides it).
    ///
    /// [`ReplicatingCallStore`]: super::store::ReplicatingCallStore
    fn scan_refs(&self, _role: PartitionRole, _primary: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Per-callRef position in a peer's compacted log.
#[derive(Clone, Debug)]
struct RefState {
    counter: u64,
    op: Op,
    partition: Partition,
    /// Set when `op == Delete`: absolute reap deadline for the tombstone.
    expiry_at_ms: Option<i64>,
}

/// A single peer's compacted changelog + its subscriber notify + liveness clock.
struct PeerLog {
    /// `counter → callRef`, ascending — the drain order. Compacted: exactly one
    /// counter per live callRef.
    entries: BTreeMap<u64, String>,
    /// `callRef → RefState` — the latest counter/op/partition for the ref.
    by_ref: HashMap<String, RefState>,
    /// Wakes the (S5) server loop when a new entry lands. Edge-triggered.
    notify: Arc<Notify>,
    /// `now_ms` of the last [`bump`](Changelog::bump) — drives idle reaping.
    last_active_ms: i64,
    /// Count of live [`serve_replog`] subscribers parked on `notify`. A peer log
    /// with `subscribers > 0` is NOT reaped while idle, so a parked server never
    /// ends up holding an orphaned `Notify` after a reap+recreate.
    ///
    /// [`serve_replog`]: super::server::ReplServer
    subscribers: usize,
    /// Highest counter that has been DROPPED by tombstone reaping (a delete the
    /// puller may have missed). A warm puller resuming from `since.counter` below
    /// this has fallen off the compacted tail and must be told to re-bootstrap
    /// ([`needs_reset`](Changelog::needs_reset)). NOT raised by compaction-moves
    /// in [`bump`](Changelog::bump) — those keep the ref live at a higher counter.
    retained_floor: u64,
}

impl PeerLog {
    fn new(now_ms: i64) -> Self {
        Self {
            entries: BTreeMap::new(),
            by_ref: HashMap::new(),
            notify: Arc::new(Notify::new()),
            last_active_ms: now_ms,
            subscribers: 0,
            retained_floor: 0,
        }
    }
}

/// RAII handle for a [`serve_replog`] subscription. Holds the peer's `Notify`
/// and keeps the peer log reap-immune for its lifetime; decrements the
/// subscriber count on drop.
///
/// [`serve_replog`]: super::server::ReplServer
pub struct Subscription {
    changelog: Changelog,
    peer: String,
    notify: Arc<Notify>,
}

impl Subscription {
    /// Await the next changelog bump for this peer (edge-triggered).
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut inner = self.changelog.inner.lock().unwrap();
        if let Some(log) = inner.peers.get_mut(&self.peer) {
            log.subscribers = log.subscribers.saturating_sub(1);
        }
    }
}

struct Inner {
    counter: u64,
    peers: HashMap<String, PeerLog>,
}

/// Node-global monotonic changelog over per-peer compacted ref-logs.
///
/// Clone-cheap (one `Arc`); share across the call path and the (S5) server loop.
#[derive(Clone)]
pub struct Changelog {
    gen: u64,
    clock: Clock,
    tombstone_ttl_ms: i64,
    dead_peer_ttl_ms: i64,
    inner: Arc<Mutex<Inner>>,
}

impl Changelog {
    /// Build a changelog under incarnation `gen` (mirror `IdGen::seeded` — the
    /// real source is deferred to S11) with `clock` for TTL math.
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

    /// The changelog position of `call_ref`'s latest entry in `peer`'s log, or
    /// `None` if this node has never bumped that ref for that peer — a monotonic
    /// position in this node's own changelog-counter domain (never a wall-clock).
    /// (General accessor; the X11 `Deactivate` watermark handshake that once used it
    /// was removed in ADR-0014 — reconciliation is now the `(p,b)` version vector
    /// and the backup self-releases on transaction completion.)
    pub fn position_of(&self, peer: &str, call_ref: &str) -> Option<Watermark> {
        let inner = self.inner.lock().unwrap();
        inner
            .peers
            .get(peer)?
            .by_ref
            .get(call_ref)
            .map(|st| Watermark::new(self.gen, st.counter))
    }

    /// Subscribe to a peer's new-entry notifications (S5's server loop awaits
    /// this). Creates the peer log if absent and increments its subscriber count
    /// so an idle [`reap`](Changelog::reap) cannot drop the log (and orphan the
    /// `Notify`) while a server is parked on it. The returned [`Subscription`]
    /// decrements on drop.
    pub fn subscribe(&self, peer: &str) -> Subscription {
        let notify = {
            let mut inner = self.inner.lock().unwrap();
            let now = self.clock.now_ms();
            let log = inner
                .peers
                .entry(peer.to_string())
                .or_insert_with(|| PeerLog::new(now));
            log.subscribers += 1;
            log.notify.clone()
        };
        Subscription {
            changelog: self.clone(),
            peer: peer.to_string(),
            notify,
        }
    }

    /// Whether a warm puller resuming from `since` has fallen off the compacted
    /// tail and must re-bootstrap (the server emits `ResetToBootstrap`). True iff
    /// `since` is same-incarnation (`since.gen >= self.gen`; a lower gen is a
    /// cold pull that re-hydrates fully anyway) **and** `since.counter` is below
    /// the peer's `retained_floor` — i.e. a reaped tombstone the puller never saw.
    pub fn needs_reset(&self, peer: &str, since: Watermark) -> bool {
        if since.gen < self.gen {
            return false;
        }
        let inner = self.inner.lock().unwrap();
        inner
            .peers
            .get(peer)
            .map(|log| since.counter < log.retained_floor)
            .unwrap_or(false)
    }

    /// Record a mutation for `peer`: assign the next counter, **compact** (drop
    /// the ref's previous counter), set its op/partition, then notify the peer's
    /// subscriber non-blocking.
    ///
    /// Op transition: first appearance keeps [`Op::Create`]; a later non-delete
    /// bump becomes [`Op::Update`]; a `Op::Delete` bump sets a tombstone with
    /// `expiry_at_ms = now + tombstone_ttl`.
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

        // Compaction: a re-update MOVES the ref to the new counter; the old slot
        // is removed so the log never grows past the live set.
        let prev_op = if let Some(prev) = log.by_ref.get(call_ref) {
            log.entries.remove(&prev.counter);
            Some(prev.op)
        } else {
            None
        };

        let effective_op = match op {
            Op::Delete => Op::Delete,
            // First sighting → Create; any later content bump → Update.
            _ => match prev_op {
                None => Op::Create,
                Some(_) => Op::Update,
            },
        };

        let expiry_at_ms = match effective_op {
            Op::Delete => Some(now + self.tombstone_ttl_ms),
            _ => None,
        };

        log.entries.insert(c, call_ref.to_string());
        log.by_ref.insert(
            call_ref.to_string(),
            RefState {
                counter: c,
                op: effective_op,
                partition,
                expiry_at_ms,
            },
        );

        // Non-blocking wake (mirrors BufferedTerminateWriter): never await a
        // subscriber, never touch a socket on the call path.
        log.notify.notify_one();
    }

    /// Drain a peer's entries with `counter > since.counter` (or **all** entries
    /// when `since.gen < self.gen` — the reboot / cold-pull case), ascending by
    /// counter, as [`Frame::Data`]. Bodies are read **live** from `source` with
    /// the changelog lock **dropped**.
    ///
    /// `role`/`primary` locate the body in the store's keyspace. A `Delete` ref
    /// or a vanished body yields `Frame::Data{ op: Delete, body: None, .. }`.
    pub async fn drain_since(
        &self,
        peer: &str,
        since: Watermark,
        source: &dyn BodySource,
        role: PartitionRole,
        primary: &str,
    ) -> Vec<Frame> {
        // Brief lock: snapshot the due (counter, callRef, op, partition) tuples,
        // then DROP the guard before any body read/await.
        let due: Vec<(u64, String, Op, Partition)> = {
            let inner = self.inner.lock().unwrap();
            let Some(log) = inner.peers.get(peer) else {
                return Vec::new();
            };
            let cold = since.gen < self.gen;
            log.entries
                .iter()
                .filter(|(c, _)| cold || **c > since.counter)
                .map(|(c, call_ref)| {
                    let st = &log.by_ref[call_ref];
                    (*c, call_ref.clone(), st.op, st.partition)
                })
                .collect()
        };

        let mut frames = Vec::with_capacity(due.len());
        for (counter, call_ref, op, partition) in due {
            let at = Watermark::new(self.gen, counter);
            if op == Op::Delete {
                frames.push(Frame::Data {
                    at,
                    op: Op::Delete,
                    partition,
                    call_ref,
                    call_gen: 0,
                    call_bgen: 0,
                    body_ttl_ms: 0,
                    indexes: Vec::new(),
                    body: None,
                });
                continue;
            }
            // Body read at send time — lock already dropped.
            let body = source.read_body(role, primary, &call_ref).await;
            match (body, source.read_meta(&call_ref)) {
                (Some(body), Some(meta)) => frames.push(Frame::Data {
                    at,
                    op,
                    partition,
                    call_ref,
                    call_gen: meta.call_gen,
                    call_bgen: meta.call_bgen,
                    body_ttl_ms: meta.body_ttl_ms,
                    indexes: meta.indexes,
                    body: Some(body),
                }),
                // Gone between snapshot and read → emit a delete.
                _ => frames.push(Frame::Data {
                    at,
                    op: Op::Delete,
                    partition,
                    call_ref,
                    call_gen: 0,
                    call_bgen: 0,
                    body_ttl_ms: 0,
                    indexes: Vec::new(),
                    body: None,
                }),
            }
        }
        frames
    }

    /// The callRefs due for `peer` with `counter > since.counter` (or **all**
    /// on a cold pull, `since.gen < self.gen`), ascending by counter. The S5
    /// server uses this to resolve each ref's `(role, primary)` keyspace before
    /// draining bodies. Brief lock; no body read.
    pub(crate) fn due_refs(&self, peer: &str, since: Watermark) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let Some(log) = inner.peers.get(peer) else {
            return Vec::new();
        };
        let cold = since.gen < self.gen;
        log.entries
            .iter()
            .filter(|(c, _)| cold || **c > since.counter)
            .map(|(_, call_ref)| call_ref.clone())
            .collect()
    }

    /// Drop a peer's whole changelog (auto-clean on disconnect; it re-bootstraps
    /// on return). Returns `true` if a log existed.
    pub fn drop_peer(&self, peer: &str) -> bool {
        self.inner.lock().unwrap().peers.remove(peer).is_some()
    }

    /// `(entries, peers)` outbound-buffer depth for the memory-attribution
    /// gauges: total compacted changelog entries summed across every peer log,
    /// and the live peer-log count. Compaction keeps one entry per live callRef
    /// per peer, so `entries` ≈ `peers × distinct-live-refs`; a peer whose
    /// entries grow without draining (slow/dead subscriber) is an outbound leak
    /// distinct from the call map. One brief lock; pure read.
    pub fn depth(&self) -> (u64, u64) {
        let inner = self.inner.lock().unwrap();
        let entries: usize = inner.peers.values().map(|p| p.entries.len()).sum();
        (entries as u64, inner.peers.len() as u64)
    }

    /// Evict expired tombstones and idle peers. Call after advancing the clock
    /// (lazy TTL — deterministic, no background task, no `DelayQueue` aliasing).
    pub fn reap(&self, now_ms: i64) {
        let dead_peer_ttl = self.dead_peer_ttl_ms;
        let mut inner = self.inner.lock().unwrap();
        // Drop idle peers wholesale — but NEVER one with a live subscriber (a
        // parked server would otherwise keep an orphaned Notify and miss bumps).
        inner
            .peers
            .retain(|_, log| log.subscribers > 0 || now_ms - log.last_active_ms < dead_peer_ttl);
        // Reap expired tombstones from surviving peers, raising the retention
        // floor: a warm puller below a reaped counter has missed that delete and
        // must re-bootstrap (`needs_reset`).
        for log in inner.peers.values_mut() {
            let expired: Vec<String> = log
                .by_ref
                .iter()
                .filter(|(_, st)| matches!(st.expiry_at_ms, Some(e) if now_ms >= e))
                .map(|(call_ref, _)| call_ref.clone())
                .collect();
            for call_ref in expired {
                if let Some(st) = log.by_ref.remove(&call_ref) {
                    log.entries.remove(&st.counter);
                    log.retained_floor = log.retained_floor.max(st.counter);
                }
            }
        }
    }

    /// Number of live entries in a peer's log (test introspection).
    #[cfg(test)]
    pub fn peer_len(&self, peer: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .peers
            .get(peer)
            .map(|l| l.entries.len())
            .unwrap_or(0)
    }

    /// Whether a peer log exists (test introspection).
    #[cfg(test)]
    pub fn has_peer(&self, peer: &str) -> bool {
        self.inner.lock().unwrap().peers.contains_key(peer)
    }
}
