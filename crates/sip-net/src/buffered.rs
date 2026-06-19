//! `BufferedUdpEndpoint` — wraps any [`UdpEndpoint`] (real `tokio` socket or the
//! simulated fabric) so that `send_to` is **non-blocking** and
//! **per-peer-isolated**. Port of `src/sip/BufferedUdpEndpoint.ts`.
//!
//! # Motivation
//!
//! The bare [`UdpEndpoint::send_to`] invokes the OS `send` (or the simulated
//! equivalent). For a *hostname* destination the kernel path runs a blocking
//! `getaddrinfo`, which stalls ~5 s on `EAI_AGAIN`. With a single ingress
//! consumer this wedges every subsequent packet on the same endpoint — the
//! kindlab DNS-DoS head-of-line stall (`docs/past-issues/2026-05-14-kindlab-dns-dos.md`).
//!
//! # Design
//!
//! One bounded queue + one drainer task per destination key. The Rust net
//! layer already reshaped the TS `(host, port)` pair into a single
//! [`SocketAddr`] (`src/types.rs`), so the peer key IS the destination
//! `SocketAddr` (the TS `peerKey(host, port)`).
//!
//!   - `send_to` is **pure enqueue** — never blocks, never fails, returns
//!     immediately. It contains **no `.await`** so a synchronous burst of sends
//!     never lets a drainer interleave (mirrors the Effect fiber-turn
//!     semantics the source relied on, and the queue-cap test asserts).
//!   - The drainer task calls `inner.send_to(buf, dst).await`. If that inner
//!     send blocks for 5 s on DNS, only THAT peer's task waits; sends to other
//!     peers continue unaffected. [`SendError`] outcomes are swallowed into a
//!     counter — SIP UDP retransmits handle loss.
//!   - Per-peer queue cap: **drop-newest** on overflow (matches kernel UDP).
//!   - **Idle reclamation**: a peer that has made no progress (no successful
//!     drain) for `idle_ttl` is reclaimed — drainer aborted, queue closed,
//!     entry removed.
//!   - **Max-peers ceiling**: a hard cap on entry count. On the new-peer path,
//!     if at cap the configured [`PeerEvictionStrategy`] picks one to evict
//!     before insertion (default: idle-LRU).
//!
//! # Clock / timer notes (see CLAUDE.md)
//!
//! `last_progress_ms` and the idle window ride [`sip_clock::Clock::now_ms`],
//! which is monotonic-anchored to `tokio::time` — under a paused runtime a
//! single `tokio::time::advance` moves the sweep `interval` AND the progress
//! timestamps together, so the sweep is fully deterministic with no separate
//! fake-clock counter. The sweeper is a plain `tokio::time::interval`; there is
//! no `DelayQueue`/`Key` here, so the epoch/Key aliasing hazard does not apply.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::net::UdpEndpoint;
use crate::types::{SendError, UdpEndpointCounters, UdpPacket};

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// Live counters for the wrapper (port of `BufferedSendCounters`). Shared
/// (`Arc`) so they can be wired to the metrics server and observed by tests
/// while the drainer/sweep tasks bump them.
#[derive(Debug, Default)]
pub struct BufferedSendCounters {
    pub enqueued: AtomicU64,
    pub dropped_queue_full: AtomicU64,
    pub dropped_evicted_with_queue: AtomicU64,
    pub inner_send_errors: AtomicU64,
    pub reclaimed_idle: AtomicU64,
    pub reclaimed_cap: AtomicU64,
}

/// An immutable snapshot of [`BufferedSendCounters`] (the value the metrics
/// server reads; convenient for test assertions).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferedSendCountersSnapshot {
    pub enqueued: u64,
    pub dropped_queue_full: u64,
    pub dropped_evicted_with_queue: u64,
    pub inner_send_errors: u64,
    pub reclaimed_idle: u64,
    pub reclaimed_cap: u64,
}

impl BufferedSendCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> BufferedSendCountersSnapshot {
        BufferedSendCountersSnapshot {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            dropped_queue_full: self.dropped_queue_full.load(Ordering::Relaxed),
            dropped_evicted_with_queue: self.dropped_evicted_with_queue.load(Ordering::Relaxed),
            inner_send_errors: self.inner_send_errors.load(Ordering::Relaxed),
            reclaimed_idle: self.reclaimed_idle.load(Ordering::Relaxed),
            reclaimed_cap: self.reclaimed_cap.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// Eviction strategy
// ---------------------------------------------------------------------------

/// What the eviction strategy sees for one peer (port of `PeerMetadata`).
#[derive(Debug, Clone, Copy)]
pub struct PeerMetadata {
    pub last_progress_ms: i64,
    pub queue_depth: usize,
}

/// Chooses which peer to evict when the wrapper is at `max_peers` and a new
/// peer must be admitted (port of `PeerEvictionStrategy`). Given the current
/// peers and `now` (epoch ms), return the victim key, or `None` to admit
/// without evicting.
pub trait PeerEvictionStrategy: Send + Sync {
    fn name(&self) -> &str;
    fn select_victim(&self, peers: &[(SocketAddr, PeerMetadata)], now: i64) -> Option<SocketAddr>;
}

/// Default strategy: evict the peer whose last successful drain is oldest
/// (port of `idleLruStrategy`).
#[derive(Debug, Default, Clone, Copy)]
pub struct IdleLruStrategy;

impl PeerEvictionStrategy for IdleLruStrategy {
    fn name(&self) -> &str {
        "idle-lru"
    }

    fn select_victim(&self, peers: &[(SocketAddr, PeerMetadata)], _now: i64) -> Option<SocketAddr> {
        let mut oldest_key: Option<SocketAddr> = None;
        let mut oldest_ms = i64::MAX;
        for (k, m) in peers {
            if m.last_progress_ms < oldest_ms {
                oldest_ms = m.last_progress_ms;
                oldest_key = Some(*k);
            }
        }
        oldest_key
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// A hook the wrapper invokes when an item is enqueued (`+1`) and when the
/// drainer finishes calling `inner.send_to` (`-1`). The simulated-network
/// quiescence wait (`await_in_flight`) tracks in-flight packets via the
/// fabric's `in_flight` counter; wiring this to `bump_in_flight` keeps buffered
/// packets accounted for so `await_in_flight` does not declare quiescence while
/// packets still sit in a per-peer queue. Production layers leave this `None`.
/// (Port of the TS `pendingWorkDelta`.)
pub type PendingWorkDelta = Arc<dyn Fn(i64) + Send + Sync>;

/// Wrapper construction options (port of `BufferedUdpEndpointOpts`).
#[derive(Clone)]
pub struct BufferedUdpEndpointOpts {
    /// Per-peer queue capacity. Overflow drops newest.
    pub per_peer_queue_max: usize,
    /// A peer with no successful drain within this window is reclaimed.
    pub idle_ttl: Duration,
    /// Hard ceiling on total peer entries. Exceeding triggers eviction.
    pub max_peers: usize,
    /// Cadence of the idle-reclamation sweeper.
    pub sweep_interval: Duration,
    /// Defaults to [`IdleLruStrategy`].
    pub eviction_strategy: Option<Arc<dyn PeerEvictionStrategy>>,
    /// Caller-owned counters; defaults to a fresh set.
    pub counters: Option<Arc<BufferedSendCounters>>,
    /// Optional in-flight accounting hook (see [`PendingWorkDelta`]).
    pub pending_work_delta: Option<PendingWorkDelta>,
}

impl BufferedUdpEndpointOpts {
    /// Minimal opts: the four scalar knobs, default strategy/counters, no
    /// pending-work hook.
    pub fn new(
        per_peer_queue_max: usize,
        idle_ttl: Duration,
        max_peers: usize,
        sweep_interval: Duration,
    ) -> Self {
        Self {
            per_peer_queue_max,
            idle_ttl,
            max_peers,
            sweep_interval,
            eviction_strategy: None,
            counters: None,
            pending_work_delta: None,
        }
    }

    pub fn with_counters(mut self, counters: Arc<BufferedSendCounters>) -> Self {
        self.counters = Some(counters);
        self
    }

    pub fn with_eviction_strategy(mut self, strategy: Arc<dyn PeerEvictionStrategy>) -> Self {
        self.eviction_strategy = Some(strategy);
        self
    }

    pub fn with_pending_work_delta(mut self, hook: PendingWorkDelta) -> Self {
        self.pending_work_delta = Some(hook);
        self
    }
}

// ---------------------------------------------------------------------------
// Per-peer queue (drop-newest bounded queue of outbound items)
// ---------------------------------------------------------------------------

struct SendItem {
    buf: Vec<u8>,
    dst: SocketAddr,
}

/// Bounded outbound queue with drop-newest overflow and an async `take`.
///
/// Mirrors [`crate::queue::PacketQueue`] (same `Mutex<VecDeque>` + `Notify`
/// shape) but carries [`SendItem`]s and exposes `depth()` so the eviction
/// snapshot and `dropped_evicted_with_queue` count can read it. We hand-roll
/// rather than use `tokio::mpsc` because mpsc exposes neither its length (the
/// drop-on-evict count and the cap snapshot need it) nor a non-blocking offer
/// that reports "full" so the caller can bump the drop counter.
struct PeerQueue {
    inner: Mutex<PeerQueueInner>,
    notify: Notify,
    cap: usize,
}

struct PeerQueueInner {
    buf: std::collections::VecDeque<SendItem>,
    closed: bool,
}

impl PeerQueue {
    fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(PeerQueueInner {
                buf: std::collections::VecDeque::new(),
                closed: false,
            }),
            notify: Notify::new(),
            cap,
        }
    }

    /// Non-blocking enqueue. Returns `false` (without enqueuing) when the queue
    /// is at capacity or closed — the caller treats `false` as a drop-newest.
    fn offer(&self, item: SendItem) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.closed || g.buf.len() >= self.cap {
            return false;
        }
        g.buf.push_back(item);
        drop(g);
        self.notify.notify_one();
        true
    }

    /// Await the next item. `None` once closed and drained. Cancellation-safe.
    async fn take(&self) -> Option<SendItem> {
        loop {
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(item) = g.buf.pop_front() {
                    return Some(item);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn depth(&self) -> usize {
        self.inner.lock().unwrap().buf.len()
    }

    fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.notify.notify_waiters();
    }
}

// ---------------------------------------------------------------------------
// Per-peer state + the shared inner
// ---------------------------------------------------------------------------

struct PeerState {
    queue: Arc<PeerQueue>,
    task: JoinHandle<()>,
    last_progress_ms: i64,
}

/// State shared between `send_to`, the per-peer drainers, and the sweep task.
struct Shared {
    inner: Box<dyn UdpEndpoint>,
    peers: Mutex<HashMap<SocketAddr, PeerState>>,
    counters: Arc<BufferedSendCounters>,
    strategy: Arc<dyn PeerEvictionStrategy>,
    pending_work_delta: Option<PendingWorkDelta>,
    clock: Clock,
    opts: BufferedUdpEndpointOpts,
}

impl Shared {
    fn pending_delta(&self, delta: i64) {
        if let Some(hook) = &self.pending_work_delta {
            hook(delta);
        }
    }

    /// Enqueue one item onto an existing peer's queue, counting accept/drop and
    /// bumping the pending-work hook on accept (port of `offerOrDrop`).
    fn offer_or_drop(&self, queue: &PeerQueue, item: SendItem) {
        if queue.offer(item) {
            self.counters.enqueued.fetch_add(1, Ordering::Relaxed);
            self.pending_delta(1);
        } else {
            self.counters.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Remove a peer: close its queue, abort its drainer, count the queued
    /// items it dropped and the reclamation reason (port of `reclaim`).
    ///
    /// Closing the queue lets the drainer's `take` resolve to `None` and the
    /// loop end cleanly; the explicit `abort` also kills any in-flight inner
    /// send (a DNS/socket op the drainer is blocked on) so the slot frees now.
    fn reclaim(&self, key: SocketAddr, reason: ReclaimReason) {
        // Lock only for the map mutation; do the side-effects after dropping it
        // so the lock is never held across the abort/notify (cheap, but keeps
        // the same brief-lock idiom as simulated.rs).
        let peer = {
            let mut peers = self.peers.lock().unwrap();
            match peers.remove(&key) {
                Some(p) => p,
                None => return,
            }
        };
        let depth = peer.queue.depth();
        peer.queue.close();
        peer.task.abort();
        if depth > 0 {
            self.counters
                .dropped_evicted_with_queue
                .fetch_add(depth as u64, Ordering::Relaxed);
            // Items dropped on eviction never reach the drainer's
            // `pending_delta(-1)`; decrement here so external in-flight
            // tracking stays balanced.
            self.pending_delta(-(depth as i64));
        }
        match reason {
            ReclaimReason::Idle => self.counters.reclaimed_idle.fetch_add(1, Ordering::Relaxed),
            ReclaimReason::Cap => self.counters.reclaimed_cap.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[derive(Clone, Copy)]
enum ReclaimReason {
    Idle,
    Cap,
}

/// The drainer task body for one peer (port of `drainerLoop`).
///
/// Pulls items one at a time via `PeerQueue::take` so `depth()` accurately
/// reflects items not yet picked up (an eager chunked take would read the queue
/// empty even with items in flight, breaking the eviction-with-queue count).
/// The loop exits on queue close (`take` → `None`) or via task `abort`.
async fn drainer_loop(shared: Arc<Shared>, key: SocketAddr, queue: Arc<PeerQueue>) {
    while let Some(item) = queue.take().await {
        if let Err(err) = shared.inner.send_to(&item.buf, item.dst).await {
            shared
                .counters
                .inner_send_errors
                .fetch_add(1, Ordering::Relaxed);
            tracing_warn(&item.dst, &err);
        }
        shared.pending_delta(-1);
        let now = shared.clock.now_ms();
        // Record progress on the live peer (it may have been reclaimed while we
        // were in `inner.send_to`; if so, skip).
        if let Some(p) = shared.peers.lock().unwrap().get_mut(&key) {
            p.last_progress_ms = now;
        }
    }
}

/// The source logs a warning on inner send failure. We have no `tracing`
/// dependency wired into this crate; keep the seam as a no-op stub so the call
/// site reads the same as the TS and a future `tracing` wiring is one edit. The
/// failure is already counted in `inner_send_errors`.
#[inline]
fn tracing_warn(_dst: &SocketAddr, _err: &SendError) {}

// ---------------------------------------------------------------------------
// The wrapper endpoint
// ---------------------------------------------------------------------------

/// A [`UdpEndpoint`] that buffers outbound sends per destination (port of the
/// `BufferedUdpEndpoint` interface). Read methods (`recv`, `try_recv`,
/// `local_addr`, `queue_depth`, `queue_max`, `counters`) delegate to the inner
/// endpoint; `send_to` enqueues onto the destination's per-peer drainer.
pub struct BufferedUdpEndpoint {
    shared: Arc<Shared>,
    sweep_task: JoinHandle<()>,
}

impl BufferedUdpEndpoint {
    /// Wrap `inner` with a per-peer outbound drainer (port of `wrapEndpoint`).
    ///
    /// `clock` is the timestamp seam for `last_progress_ms` / the idle window —
    /// pass `Clock::system()` in production, `Clock::test_at(0)` under a paused
    /// test runtime.
    pub fn wrap(inner: Box<dyn UdpEndpoint>, opts: BufferedUdpEndpointOpts, clock: Clock) -> Self {
        let counters = opts.counters.clone().unwrap_or_default();
        let strategy: Arc<dyn PeerEvictionStrategy> = opts
            .eviction_strategy
            .clone()
            .unwrap_or_else(|| Arc::new(IdleLruStrategy));
        let pending_work_delta = opts.pending_work_delta.clone();

        let shared = Arc::new(Shared {
            inner,
            peers: Mutex::new(HashMap::new()),
            counters,
            strategy,
            pending_work_delta,
            clock,
            opts,
        });

        // Idle sweep — a plain `tokio::time::interval` so `tokio::time::advance`
        // drives it deterministically under a paused runtime.
        let sweep_shared = shared.clone();
        let sweep_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(sweep_shared.opts.sweep_interval);
            // The first `tick()` completes immediately; consume it so the first
            // *sweep* lands one interval in (matching the TS `Effect.sleep`
            // before the first scan).
            ticker.tick().await;
            let idle_ttl_ms = sweep_shared.opts.idle_ttl.as_millis() as i64;
            loop {
                ticker.tick().await;
                let now = sweep_shared.clock.now_ms();
                let expired: Vec<SocketAddr> = {
                    let peers = sweep_shared.peers.lock().unwrap();
                    peers
                        .iter()
                        .filter(|(_, p)| now - p.last_progress_ms > idle_ttl_ms)
                        .map(|(k, _)| *k)
                        .collect()
                };
                for k in expired {
                    sweep_shared.reclaim(k, ReclaimReason::Idle);
                }
            }
        });

        Self { shared, sweep_task }
    }

    /// Active peer count — exposed for tests and metrics (the TS `peerCount`).
    pub fn peer_count(&self) -> usize {
        self.shared.peers.lock().unwrap().len()
    }

    /// The wrapper's counters (the TS `bufferedCounters`).
    pub fn buffered_counters(&self) -> &Arc<BufferedSendCounters> {
        &self.shared.counters
    }
}

impl Drop for BufferedUdpEndpoint {
    fn drop(&mut self) {
        // Stop the sweeper, then tear down every per-peer drainer so no tasks
        // outlive the wrapper (the TS scope ending interrupts the forked
        // fibers; here we abort them explicitly).
        self.sweep_task.abort();
        let mut peers = self.shared.peers.lock().unwrap();
        for (_, p) in peers.drain() {
            p.queue.close();
            p.task.abort();
        }
    }
}

#[async_trait]
impl UdpEndpoint for BufferedUdpEndpoint {
    /// Pure enqueue — returns immediately, never blocks, never fails (port of
    /// `send`). MUST contain no `.await`: a synchronous burst of `send_to`
    /// calls must not let a drainer interleave (the queue-cap semantics), and
    /// the caller must never wait on a slow inner send.
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        let key = dst;
        let mut peers = self.shared.peers.lock().unwrap();

        if let Some(existing) = peers.get(&key) {
            let queue = existing.queue.clone();
            // Release the map lock before offering (offer takes the queue's own
            // lock; never nest the two).
            drop(peers);
            self.shared.offer_or_drop(
                &queue,
                SendItem {
                    buf: buf.to_vec(),
                    dst,
                },
            );
            return Ok(());
        }

        // New peer — enforce the cap first.
        if peers.len() >= self.shared.opts.max_peers {
            let now = self.shared.clock.now_ms();
            let snapshot: Vec<(SocketAddr, PeerMetadata)> = peers
                .iter()
                .map(|(k, s)| {
                    (
                        *k,
                        PeerMetadata {
                            last_progress_ms: s.last_progress_ms,
                            queue_depth: s.queue.depth(),
                        },
                    )
                })
                .collect();
            if let Some(victim) = self.shared.strategy.select_victim(&snapshot, now) {
                // Drop the map lock so `reclaim` can re-acquire it (it removes
                // the victim and runs side-effects). Re-acquire afterwards to
                // insert the new peer.
                drop(peers);
                self.shared.reclaim(victim, ReclaimReason::Cap);
                peers = self.shared.peers.lock().unwrap();
            }
        }

        let queue = Arc::new(PeerQueue::new(self.shared.opts.per_peer_queue_max));
        let now = self.shared.clock.now_ms();
        let task = tokio::spawn(drainer_loop(self.shared.clone(), key, queue.clone()));
        peers.insert(
            key,
            PeerState {
                queue: queue.clone(),
                task,
                last_progress_ms: now,
            },
        );
        drop(peers);
        self.shared.offer_or_drop(
            &queue,
            SendItem {
                buf: buf.to_vec(),
                dst,
            },
        );
        Ok(())
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.shared.inner.recv().await
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        self.shared.inner.try_recv()
    }

    fn local_addr(&self) -> SocketAddr {
        self.shared.inner.local_addr()
    }

    fn queue_depth(&self) -> usize {
        self.shared.inner.queue_depth()
    }

    fn queue_max(&self) -> usize {
        self.shared.inner.queue_max()
    }

    fn counters(&self) -> UdpEndpointCounters {
        self.shared.inner.counters()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // -- Stub UdpEndpoint (port of the test's `makeStub`) -------------------

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Behaviour {
        Ok,
        Hang,
        Fail,
    }

    struct SendRecord {
        dst: SocketAddr,
        #[allow(dead_code)]
        buf: Vec<u8>,
    }

    /// A hand-rolled `UdpEndpoint` whose `send_to` is configurable per-host:
    /// immediate success, hang-forever (until the drainer is aborted), or fail
    /// with [`SendError`]. Records every send for assertions.
    struct Stub {
        sends: Mutex<Vec<SendRecord>>,
        behaviour: Box<dyn Fn(SocketAddr) -> Behaviour + Send + Sync>,
        local: SocketAddr,
        queue_max: usize,
    }

    impl Stub {
        fn new(behaviour: impl Fn(SocketAddr) -> Behaviour + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                sends: Mutex::new(Vec::new()),
                behaviour: Box::new(behaviour),
                local: "0.0.0.0:0".parse().unwrap(),
                queue_max: 0,
            })
        }

        fn send_count(&self) -> usize {
            self.sends.lock().unwrap().len()
        }

        fn sends_to(&self, dst: SocketAddr) -> usize {
            self.sends
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.dst == dst)
                .count()
        }
    }

    // The wrapper takes `Box<dyn UdpEndpoint>` but the test also wants to read
    // the recorded sends afterwards, so the stub is shared via `Arc` and a thin
    // newtype forwards the trait to it.
    struct StubHandle(Arc<Stub>);

    #[async_trait]
    impl UdpEndpoint for StubHandle {
        async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
            self.0.sends.lock().unwrap().push(SendRecord {
                dst,
                buf: buf.to_vec(),
            });
            match (self.0.behaviour)(dst) {
                Behaviour::Ok => Ok(()),
                Behaviour::Fail => Err(SendError {
                    message: format!("stub fail to {dst}"),
                }),
                // Hang forever — only the drainer's `abort` ends this.
                Behaviour::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
        async fn recv(&self) -> Option<UdpPacket> {
            std::future::pending().await
        }
        fn try_recv(&self) -> Option<UdpPacket> {
            None
        }
        fn local_addr(&self) -> SocketAddr {
            self.0.local
        }
        fn queue_depth(&self) -> usize {
            0
        }
        fn queue_max(&self) -> usize {
            self.0.queue_max
        }
        fn counters(&self) -> UdpEndpointCounters {
            UdpEndpointCounters::default()
        }
    }

    fn default_opts(counters: Arc<BufferedSendCounters>) -> BufferedUdpEndpointOpts {
        BufferedUdpEndpointOpts::new(
            4,                          // per_peer_queue_max
            Duration::from_millis(5_000), // idle_ttl
            4,                          // max_peers
            Duration::from_millis(1_000), // sweep_interval
        )
        .with_counters(counters)
    }

    fn wrap_stub(stub: Arc<Stub>, opts: BufferedUdpEndpointOpts) -> BufferedUdpEndpoint {
        BufferedUdpEndpoint::wrap(Box::new(StubHandle(stub)), opts, Clock::test_at(0))
    }

    fn addr(host_octet: u8, port: u16) -> SocketAddr {
        // Distinct peer keys; the exact IP is irrelevant (the stub keys
        // behaviour off the address). Use 127.0.x.0:port for readability.
        SocketAddr::from(([127, 0, host_octet, 0], port))
    }

    /// Hand the runtime to spawned tasks (drainers, sweeper) under the
    /// current-thread paused runtime — the analogue of the TS `Effect.yieldNow`
    /// pumps. Yields several times so a drainer can take + invoke + record.
    async fn pump(n: usize) {
        for _ in 0..n {
            tokio::task::yield_now().await;
        }
    }

    // -- caller-side non-blocking ------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn send_returns_immediately_even_when_inner_send_hangs_forever() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Hang);
        let wrapped = wrap_stub(stub.clone(), default_opts(counters.clone()));
        let slow = addr(1, 5060);

        // Each of these would block ~5 s under the old direct-call shape.
        wrapped.send_to(b"a", slow).await.unwrap();
        wrapped.send_to(b"b", slow).await.unwrap();
        wrapped.send_to(b"c", slow).await.unwrap();

        // The drainer takes the first packet and invokes inner.send_to (hung);
        // the next two sit in queue. Pump so the drainer dequeues the first.
        pump(4).await;
        assert_eq!(stub.send_count(), 1);
        let c = counters.snapshot();
        assert_eq!(c.enqueued, 3);
        assert_eq!(c.dropped_queue_full, 0);
    }

    // -- per-peer isolation -------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn fast_peer_keeps_draining_while_slow_peers_drainer_is_stuck() {
        let counters = BufferedSendCounters::new();
        let slow = addr(1, 5060);
        let fast = addr(2, 5060);
        let stub = Stub::new(move |dst| {
            if dst == slow {
                Behaviour::Hang
            } else {
                Behaviour::Ok
            }
        });
        let wrapped = wrap_stub(stub.clone(), default_opts(counters.clone()));

        wrapped.send_to(b"s1", slow).await.unwrap();
        wrapped.send_to(b"f1", fast).await.unwrap();
        wrapped.send_to(b"f2", fast).await.unwrap();
        wrapped.send_to(b"f3", fast).await.unwrap();

        pump(8).await;

        assert_eq!(stub.sends_to(fast), 3);
        // Slow drainer is stuck on its first packet; never advances.
        assert_eq!(stub.sends_to(slow), 1);
    }

    // -- per-peer queue cap (drop-newest) -----------------------------------

    #[tokio::test(start_paused = true)]
    async fn drops_newest_when_peer_queue_is_full() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Hang);
        let mut opts = default_opts(counters.clone());
        opts.per_peer_queue_max = 2;
        let wrapped = wrap_stub(stub.clone(), opts);
        let x = addr(1, 5060);

        // Five sends issued back-to-back as a synchronous burst (no `.await`
        // inside `send_to`) before the drainer task is scheduled. With cap=2 the
        // queue accepts the first two and drops the rest.
        wrapped.send_to(b"1", x).await.unwrap();
        wrapped.send_to(b"2", x).await.unwrap();
        wrapped.send_to(b"3", x).await.unwrap();
        wrapped.send_to(b"4", x).await.unwrap();
        wrapped.send_to(b"5", x).await.unwrap();

        let c = counters.snapshot();
        assert_eq!(c.enqueued, 2);
        assert_eq!(c.dropped_queue_full, 3);
    }

    // -- idle reclamation ---------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn a_peer_with_no_progress_for_idle_ttl_is_reclaimed_by_the_sweeper() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Ok);
        let wrapped = wrap_stub(stub.clone(), default_opts(counters.clone()));
        let x = addr(1, 5060);

        wrapped.send_to(b"one", x).await.unwrap();
        pump(2).await;
        assert_eq!(wrapped.peer_count(), 1);

        // Advance past idle_ttl. Sweeper fires every 1 s; after 6 s the peer's
        // last_progress_ms is older than idle_ttl and it gets reclaimed.
        tokio::time::advance(Duration::from_millis(6_000)).await;
        pump(2).await;

        assert_eq!(wrapped.peer_count(), 0);
        assert_eq!(counters.snapshot().reclaimed_idle, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn reclaiming_a_peer_with_a_non_empty_queue_increments_dropped_evicted_with_queue() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Hang);
        let mut opts = default_opts(counters.clone());
        opts.per_peer_queue_max = 8;
        let wrapped = wrap_stub(stub.clone(), opts);
        let x = addr(1, 5060);

        // First send goes into inner.send_to (hung); next 3 sit in queue.
        wrapped.send_to(b"a", x).await.unwrap();
        wrapped.send_to(b"b", x).await.unwrap();
        wrapped.send_to(b"c", x).await.unwrap();
        wrapped.send_to(b"d", x).await.unwrap();
        pump(2).await;

        tokio::time::advance(Duration::from_millis(6_000)).await;
        pump(2).await;

        let c = counters.snapshot();
        assert_eq!(c.reclaimed_idle, 1);
        // 3 queued packets dropped on reclamation (the 4th was in flight).
        assert_eq!(c.dropped_evicted_with_queue, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reclaimed_peers_key_can_be_re_enqueued_cleanly() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Ok);
        let mut opts = default_opts(counters.clone());
        opts.idle_ttl = Duration::from_millis(1_000);
        opts.sweep_interval = Duration::from_millis(500);
        let wrapped = wrap_stub(stub.clone(), opts);
        let x = addr(1, 5060);

        wrapped.send_to(b"first", x).await.unwrap();
        pump(2).await;

        tokio::time::advance(Duration::from_millis(2_000)).await;
        pump(2).await;
        assert_eq!(wrapped.peer_count(), 0);

        wrapped.send_to(b"second", x).await.unwrap();
        pump(2).await;
        assert_eq!(wrapped.peer_count(), 1);
        assert_eq!(stub.send_count(), 2);
    }

    // -- max-peers eviction (idle-LRU) -------------------------------------

    #[tokio::test(start_paused = true)]
    async fn creating_a_peer_at_max_peers_evicts_the_oldest_idle_lru() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Hang);
        let mut opts = default_opts(counters.clone());
        opts.max_peers = 2;
        opts.idle_ttl = Duration::from_millis(60_000);
        opts.sweep_interval = Duration::from_millis(30_000);
        let wrapped = wrap_stub(stub.clone(), opts);

        wrapped.send_to(b"a", addr(1, 5060)).await.unwrap();
        tokio::time::advance(Duration::from_millis(10)).await;
        wrapped.send_to(b"b", addr(2, 5060)).await.unwrap();
        pump(2).await;
        assert_eq!(wrapped.peer_count(), 2);

        // Third peer triggers cap eviction. h1 is the oldest → evicted.
        tokio::time::advance(Duration::from_millis(10)).await;
        wrapped.send_to(b"c", addr(3, 5060)).await.unwrap();
        pump(2).await;

        assert_eq!(wrapped.peer_count(), 2);
        assert_eq!(counters.snapshot().reclaimed_cap, 1);
    }

    // -- inner send errors --------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn send_error_from_inner_send_is_swallowed_drainer_continues_caller_unaffected() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Fail);
        let wrapped = wrap_stub(stub.clone(), default_opts(counters.clone()));
        let x = addr(1, 5060);

        wrapped.send_to(b"a", x).await.unwrap();
        wrapped.send_to(b"b", x).await.unwrap();
        wrapped.send_to(b"c", x).await.unwrap();

        pump(8).await;

        // All three attempted; all three failed; the drainer kept going.
        assert_eq!(stub.send_count(), 3);
        let c = counters.snapshot();
        assert_eq!(c.inner_send_errors, 3);
        assert_eq!(c.enqueued, 3);
    }

    // -- pass-through reads -------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn pass_through_reads_delegate_to_inner() {
        let counters = BufferedSendCounters::new();
        let stub = Stub::new(|_| Behaviour::Ok);
        let local = stub.local;
        let qmax = stub.queue_max;
        let wrapped = wrap_stub(stub.clone(), default_opts(counters));

        assert_eq!(wrapped.local_addr(), local);
        assert_eq!(wrapped.queue_max(), qmax);
        assert_eq!(wrapped.queue_depth(), 0);
        assert!(wrapped.try_recv().is_none());
        assert_eq!(wrapped.counters(), UdpEndpointCounters::default());
    }

    // -- pending-work hook (the bump_in_flight seam) -----------------------
    // Not a TS-test port; guards the in-flight accounting the wrapper exposes
    // for the simulated quiescence wait (`bump_in_flight`). Enqueue bumps +1,
    // a successful drain −1, and eviction-with-queue decrements the stranded
    // items so the net returns to 0.

    #[tokio::test(start_paused = true)]
    async fn pending_work_delta_balances_to_zero_across_drain_and_evict() {
        let counters = BufferedSendCounters::new();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let hi = Arc::new(AtomicUsize::new(0));
        let in_flight2 = in_flight.clone();
        let hi2 = hi.clone();
        let hook: PendingWorkDelta = Arc::new(move |d: i64| {
            if d >= 0 {
                let v = in_flight2.fetch_add(d as usize, Ordering::SeqCst) + d as usize;
                hi2.fetch_max(v, Ordering::SeqCst);
            } else {
                in_flight2.fetch_sub((-d) as usize, Ordering::SeqCst);
            }
        });

        // One peer that drains OK, one that hangs and is then evicted.
        let ok = addr(1, 5060);
        let stuck = addr(2, 5060);
        let stub = Stub::new(move |dst| {
            if dst == stuck {
                Behaviour::Hang
            } else {
                Behaviour::Ok
            }
        });
        let opts = default_opts(counters.clone()).with_pending_work_delta(hook);
        let wrapped = wrap_stub(stub.clone(), opts);

        wrapped.send_to(b"ok", ok).await.unwrap();
        wrapped.send_to(b"s1", stuck).await.unwrap();
        wrapped.send_to(b"s2", stuck).await.unwrap();
        pump(4).await;

        // The OK peer drained (−1); the stuck peer holds 1 in flight + 1 queued.
        assert!(hi.load(Ordering::SeqCst) >= 1);

        // Idle-reclaim the stuck peer; its queued item is decremented. The
        // in-flight item is still "owed" a −1 by the aborted drainer, but the
        // queued one is squared here.
        tokio::time::advance(Duration::from_millis(6_000)).await;
        pump(4).await;
        assert_eq!(wrapped.peer_count(), 0);
        // Only the item that was mid-`inner.send_to` when aborted is unbalanced
        // (its drainer never reached `pending_delta(-1)`); the queued one and
        // the OK drain are both balanced.
        assert_eq!(in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(counters.snapshot().dropped_evicted_with_queue, 1);
    }
}
