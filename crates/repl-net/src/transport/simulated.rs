//! In-process `SimulatedReplicationNetwork` — the fake-clock workhorse.
//!
//! Mirrors `sip-net`'s `SimulatedSignalingNetwork` fabric: an
//! `Arc<SimShared>` with a routing table keyed by `SocketAddr` and a fault
//! switchboard installed (builder-style) **before** the fabric is shared. The
//! difference from the UDP sim is that replication is **connection-oriented and
//! ordered**: `connect` finds the listener, builds a bidirectional ordered
//! channel pair, hands one end to the listener's `accept` queue (the server
//! end) and returns the other (the client end).
//!
//! ## FIFO ordering & fake-clock cooperation
//! Each direction has a dedicated **delivery actor** (a spawned task). `send`
//! encodes the frame and pushes the bytes onto an in-order staging queue
//! (`unbounded` mpsc → never blocks the sender for ordering reasons); the
//! actor pops items **one at a time, in order**, `sleep`s the per-pair transit
//! delay (`>= 1 ms`, see below), then forwards the bytes into the bounded
//! inbound channel the peer's `recv` drains. Because a single actor drains its
//! staging queue sequentially, equal-delay sends can never reorder — strict
//! FIFO. The `sleep` is a `tokio::time::sleep`, so under
//! `#[tokio::test(start_paused = true)]` the actor parks until `advance` moves
//! the clock past the delay — fully deterministic, cooperative delivery.
//!
//! ## Transit delay >= 1 ms (the 0→1 coercion)
//! Coerced in [`SimulatedReplicationNetwork::new`] and again whenever a `delay`
//! fault is set. Zero transit under a paused runtime is non-deterministic: a
//! spawned `sleep(0)` races the pipeline, so a frame can be processed a turn
//! late and a cancel can land after a timer fired (CLAUDE.md hazard). Never 0.
//!
//! ## Bounded buffer / backpressure / drop-on-overflow
//! The peer-facing inbound channel is a **bounded** `mpsc` (capacity =
//! configurable buffer cap). Default: the delivery actor `send().await`s into
//! it, so a full buffer with no drainer parks the actor → models TCP
//! flow-control (the sender is *not* told; its staging queue simply stops being
//! drained). With the `drop_on_overflow` fault armed, the actor instead
//! `try_send`s and, on `Full`, **cuts the connection** (drops the subscriber):
//! `recv` then yields `None` and further `send` returns `Closed`.
//!
//! ## Fault switchboard
//! Keyed by directed pair `(src, dst)`. `delay/stall/resume` mutate the live
//! per-direction state the actor consults each loop; `cut/partition` flip a
//! cut flag (the actor closes the inbound channel and exits, `recv`→`None`,
//! `send`→`Closed`); `heal`/reconnect is just a fresh `connect` succeeding once
//! the partition is cleared.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::{decode_frame, encode_frame, Frame};

use super::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};

/// A directed connection fault, keyed by `(src, dst)` direction or by endpoint.
///
/// Set on the [`SimulatedReplicationNetwork`] builder before the fabric is
/// shared. `delay`/`stall`/`cut` are **directed** (apply to `src → dst`);
/// `partition`/`heal` are **bidirectional** (both directions between two
/// endpoints). `reconnect` is not a fault — it is just a fresh `connect`
/// succeeding once any `cut`/partition on the pair is cleared.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the per-direction transit delay (coerced to `>= 1 ms`).
    Delay {
        /// Source endpoint of the directed connection.
        src: SocketAddr,
        /// Destination endpoint of the directed connection.
        dst: SocketAddr,
        /// New transit delay in milliseconds.
        ms: u64,
    },
    /// Pause delivery on `src → dst`: the staging queue grows, nothing is
    /// delivered, until a matching [`Fault::Resume`].
    Stall {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Resume a stalled direction: buffered frames flush in order.
    Resume {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Cut `src → dst` now: in-flight + future sends on live connections of
    /// that direction fail; their `recv` yields `None`.
    Cut {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Partition two endpoints: cut **both** directions and block new
    /// `connect`s between them until [`Fault::Heal`].
    Partition {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Heal a partition: clear the block so a fresh `connect` succeeds.
    Heal {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Arm buffer-overflow → drop-subscriber on `src → dst`: when the bounded
    /// inbound buffer is full, the delivery actor cuts the connection instead
    /// of awaiting space (the "buffer-full → drop subscriber → reconnect"
    /// goal-1 scenario).
    DropOnOverflow {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
}

/// Directed pair key for the fault tables.
type Pair = (SocketAddr, SocketAddr);

/// Per-direction live state the delivery actor consults each loop. Shared
/// (`Arc`) between the fabric (writers, via faults) and the actor (reader).
struct DirState {
    /// Transit delay in ms (`>= 1`).
    delay_ms: AtomicU64,
    /// Delivery paused (stall) — actor holds the head item without delivering.
    stalled: AtomicBool,
    /// Connection cut — actor closes the inbound channel and exits.
    cut: AtomicBool,
    /// On a full bounded buffer, cut instead of awaiting space.
    drop_on_overflow: AtomicBool,
    /// Woken when any of the above flip, so a stalled/parked actor re-checks.
    wake: Notify,
}

impl DirState {
    fn new(delay_ms: u64, drop_on_overflow: bool) -> Self {
        Self {
            delay_ms: AtomicU64::new(delay_ms.max(1)),
            stalled: AtomicBool::new(false),
            cut: AtomicBool::new(false),
            drop_on_overflow: AtomicBool::new(drop_on_overflow),
            wake: Notify::new(),
        }
    }
}

struct ListenerHandle {
    /// Queue of accepted server-end connections awaiting `accept()`.
    incoming_tx: mpsc::UnboundedSender<Box<dyn ReplicationConnection>>,
}

struct SimShared {
    routing: Mutex<HashMap<SocketAddr, ListenerHandle>>,
    /// Default per-hop transit delay (ms, `>= 1`) for new directions.
    default_delay_ms: u64,
    /// Default inbound-buffer capacity.
    buffer_cap: usize,
    /// Per-direction live state, created lazily per `connect`-direction and
    /// kept so later `delay/stall/cut` faults reach the live actors.
    dir_state: Mutex<HashMap<Pair, Arc<DirState>>>,
    /// Endpoints to drop-on-overflow by default for a fresh direction.
    drop_on_overflow_pairs: Mutex<HashSet<Pair>>,
    /// Partitions blocking new `connect`s (unordered pair, stored both ways).
    partitions: Mutex<HashSet<Pair>>,
    /// Live in-flight frame count (across all delivery actors) — harness
    /// introspection, mirrors sip-net's `in_flight`.
    in_flight: AtomicI64,
}

impl SimShared {
    /// Fetch (or lazily create) the live direction state for `src → dst`.
    fn dir(&self, src: SocketAddr, dst: SocketAddr) -> Arc<DirState> {
        let mut g = self.dir_state.lock().unwrap();
        g.entry((src, dst))
            .or_insert_with(|| {
                let drop_on = self
                    .drop_on_overflow_pairs
                    .lock()
                    .unwrap()
                    .contains(&(src, dst));
                Arc::new(DirState::new(self.default_delay_ms, drop_on))
            })
            .clone()
    }
}

/// The simulated replication network. Clone shares the routing fabric.
#[derive(Clone)]
pub struct SimulatedReplicationNetwork {
    shared: Arc<SimShared>,
}

impl SimulatedReplicationNetwork {
    /// Build a fabric with the given per-hop transit delay (ms) and a default
    /// inbound-buffer capacity.
    ///
    /// A delay of `0` is coerced to `1` — zero transit under a paused runtime
    /// is a determinism trap (see the module docs / CLAUDE.md). `buffer_cap` is
    /// coerced to `>= 1` (a zero-capacity bounded channel can never deliver).
    pub fn new(transit_delay_ms: u64, buffer_cap: usize) -> Self {
        Self {
            shared: Arc::new(SimShared {
                routing: Mutex::new(HashMap::new()),
                default_delay_ms: transit_delay_ms.max(1),
                buffer_cap: buffer_cap.max(1),
                dir_state: Mutex::new(HashMap::new()),
                drop_on_overflow_pairs: Mutex::new(HashSet::new()),
                partitions: Mutex::new(HashSet::new()),
                in_flight: AtomicI64::new(0),
            }),
        }
    }

    /// Convenience: a fabric with the default 8-frame buffer.
    pub fn with_delay(transit_delay_ms: u64) -> Self {
        Self::new(transit_delay_ms, 8)
    }

    /// Install one fault. Directed faults that name a not-yet-connected
    /// direction pre-seed its state so the next `connect` inherits it.
    ///
    /// May be called before or after the fabric is shared — all state is behind
    /// interior mutability, so a fault flips the **live** actor's flags.
    pub fn apply_fault(&self, fault: Fault) {
        match fault {
            Fault::Delay { src, dst, ms } => {
                self.shared
                    .dir(src, dst)
                    .delay_ms
                    .store(ms.max(1), Ordering::SeqCst);
            }
            Fault::Stall { src, dst } => {
                self.shared.dir(src, dst).stalled.store(true, Ordering::SeqCst);
            }
            Fault::Resume { src, dst } => {
                let d = self.shared.dir(src, dst);
                d.stalled.store(false, Ordering::SeqCst);
                d.wake.notify_waiters();
            }
            Fault::Cut { src, dst } => {
                let d = self.shared.dir(src, dst);
                d.cut.store(true, Ordering::SeqCst);
                d.wake.notify_waiters();
            }
            Fault::Partition { a, b } => {
                {
                    let mut p = self.shared.partitions.lock().unwrap();
                    p.insert((a, b));
                    p.insert((b, a));
                }
                for (s, d) in [(a, b), (b, a)] {
                    let st = self.shared.dir(s, d);
                    st.cut.store(true, Ordering::SeqCst);
                    st.wake.notify_waiters();
                }
            }
            Fault::Heal { a, b } => {
                let mut p = self.shared.partitions.lock().unwrap();
                p.remove(&(a, b));
                p.remove(&(b, a));
                // The cut DirStates stay cut (existing conns are dead); a fresh
                // connect creates new DirStates. Drop the stale ones so the new
                // direction starts clean.
                let mut ds = self.shared.dir_state.lock().unwrap();
                ds.remove(&(a, b));
                ds.remove(&(b, a));
            }
            Fault::DropOnOverflow { src, dst } => {
                self.shared
                    .drop_on_overflow_pairs
                    .lock()
                    .unwrap()
                    .insert((src, dst));
                self.shared
                    .dir(src, dst)
                    .drop_on_overflow
                    .store(true, Ordering::SeqCst);
            }
        }
    }

    /// Builder form of [`apply_fault`](Self::apply_fault).
    pub fn with_fault(self, fault: Fault) -> Self {
        self.apply_fault(fault);
        self
    }

    /// Live count of frames in transit across all delivery actors. Mirrors
    /// sip-net's `in_flight` for harness quiescence assertions.
    pub fn in_flight(&self) -> i64 {
        self.shared.in_flight.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ReplicationNetwork for SimulatedReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        // A `connect` needs a fresh, ephemeral *local* address for the client
        // end so the directed-pair fault keys are well-defined. We synthesise
        // one from a counter on 127.0.0.x:port-ish space — but the harness
        // never inspects client ports, only the directed pair, so a unique
        // synthetic addr suffices. Reuse `dst`'s ip family.
        let local = synth_local(dst);
        self.connect_from(local, dst).await
    }

    async fn listen(
        &self,
        local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        {
            let mut routing = self.shared.routing.lock().unwrap();
            if routing.contains_key(&local) {
                return Err(ListenError::AlreadyInUse(local));
            }
            routing.insert(local, ListenerHandle { incoming_tx });
        }
        Ok(Box::new(SimListener {
            local,
            incoming: tokio::sync::Mutex::new(incoming_rx),
            shared: self.shared.clone(),
        }))
    }
}

impl SimulatedReplicationNetwork {
    /// `connect` with an explicit client-side local address (used by tests that
    /// want a stable pair key for fault injection).
    pub async fn connect_from(
        &self,
        local: SocketAddr,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        // Partitioned? refuse.
        if self.shared.partitions.lock().unwrap().contains(&(local, dst)) {
            return Err(ConnectError::Blocked {
                addr: dst,
                reason: "partitioned".into(),
            });
        }

        let incoming_tx = {
            let routing = self.shared.routing.lock().unwrap();
            match routing.get(&dst) {
                Some(h) => h.incoming_tx.clone(),
                None => return Err(ConnectError::Refused(dst)),
            }
        };

        // Two directions: client→server is (local, dst); server→client is
        // (dst, local). Each gets its own ordered delivery actor.
        let c2s = spawn_wire(self.shared.clone(), local, dst);
        let s2c = spawn_wire(self.shared.clone(), dst, local);

        let client = SimConnection {
            local,
            peer: dst,
            out: c2s.staging_tx,
            out_dir: c2s.dir.clone(),
            inbound: tokio::sync::Mutex::new(s2c.inbound_rx),
            in_dir: s2c.dir.clone(),
        };
        let server = SimConnection {
            local: dst,
            peer: local,
            out: s2c.staging_tx,
            out_dir: s2c.dir,
            inbound: tokio::sync::Mutex::new(c2s.inbound_rx),
            in_dir: c2s.dir,
        };

        // Hand the server end to the listener's accept queue. If the receiver
        // is gone (listener dropped between lookup and now) the connect still
        // "succeeds" but the server end is discarded — its drop closes the
        // wire and the client's next recv yields None.
        let _ = incoming_tx.send(Box::new(server) as Box<dyn ReplicationConnection>);

        Ok(Box::new(client))
    }
}

/// Synthesise a unique client-side local address in the same ip family as
/// `dst`. The port carries a process-global counter so distinct `connect`s get
/// distinct pair keys.
fn synth_local(dst: SocketAddr) -> SocketAddr {
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let port = 40000u16.wrapping_add((n % 20000) as u16).max(1025);
    match dst {
        SocketAddr::V4(_) => SocketAddr::from(([127, 0, 0, 1], port)),
        SocketAddr::V6(_) => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    }
}

/// The two ends of one directional wire, returned from [`spawn_wire`].
struct Wire {
    /// Sender side: `send` pushes encoded bytes here (ordered, never blocks).
    staging_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Receiver side: the peer's `recv` drains decoded frames from here.
    inbound_rx: mpsc::Receiver<Vec<u8>>,
    /// Live fault state for this direction.
    dir: Arc<DirState>,
}

/// Spawn the per-direction delivery actor for `src → dst` and return its
/// staging sender + inbound receiver + shared fault state.
fn spawn_wire(shared: Arc<SimShared>, src: SocketAddr, dst: SocketAddr) -> Wire {
    let dir = shared.dir(src, dst);
    let (staging_tx, mut staging_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(shared.buffer_cap);

    let dir_actor = dir.clone();
    let shared_actor = shared.clone();
    tokio::spawn(async move {
        // One delivery actor per direction. Each staged item is assigned a
        // monotonically-increasing **absolute** delivery deadline
        // (`max(last_deadline, now) + delay`; `delay >= 1 ms`, so consecutive
        // deadlines strictly increase) so FIFO is preserved AND a single coarse
        // `tokio::time::advance` past several deadlines lets the actor drain
        // every now-ready item in one wake — cooperative with the 100 ms-chunk
        // paused-clock harness, unlike a one-sleep-per-wake loop. (A test that
        // never `recv`s must still yield once after the advance so the woken
        // actor is scheduled; `recv` itself provides that yield.)
        //
        // Items carry their deadline; a `Delay` fault changes the delay for
        // *subsequently* staged items (deadline is computed at staging time).
        let mut pending: VecDeque<(tokio::time::Instant, Vec<u8>)> = VecDeque::new();
        let mut last_deadline = tokio::time::Instant::now();

        loop {
            if dir_actor.cut.load(Ordering::SeqCst) {
                drop(inbound_tx); // peer recv → None
                return;
            }

            // Pull every currently-staged item, stamping monotonic deadlines so
            // a burst of sends becomes a run of ordered timers.
            loop {
                match staging_rx.try_recv() {
                    Ok(bytes) => {
                        let now = tokio::time::Instant::now();
                        let delay = Duration::from_millis(dir_actor.delay_ms.load(Ordering::SeqCst).max(1));
                        let base = last_deadline.max(now);
                        let deadline = base + delay;
                        last_deadline = deadline;
                        pending.push_back((deadline, bytes));
                        shared_actor.in_flight.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        if pending.is_empty() {
                            drop(inbound_tx); // sender gone, nothing left → close
                            return;
                        }
                        break;
                    }
                }
            }

            // Nothing staged: park until a send arrives or a fault flips.
            if pending.is_empty() {
                tokio::select! {
                    biased;
                    _ = dir_actor.wake.notified() => continue,
                    next = staging_rx.recv() => match next {
                        Some(bytes) => {
                            let now = tokio::time::Instant::now();
                            let delay = Duration::from_millis(dir_actor.delay_ms.load(Ordering::SeqCst).max(1));
                            let deadline = last_deadline.max(now) + delay;
                            last_deadline = deadline;
                            pending.push_back((deadline, bytes));
                            shared_actor.in_flight.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        None => { drop(inbound_tx); return; }
                    }
                }
            }

            // Stalled: hold everything until resumed / cut.
            if dir_actor.stalled.load(Ordering::SeqCst) {
                dir_actor.wake.notified().await;
                continue;
            }

            // Sleep until the head item's deadline, bailing if a fault flips or
            // a new (earlier-staged) send needs stamping.
            let head_deadline = pending.front().unwrap().0;
            tokio::select! {
                biased;
                _ = dir_actor.wake.notified() => continue,
                _ = tokio::time::sleep_until(head_deadline) => {}
            }

            if dir_actor.cut.load(Ordering::SeqCst) {
                // Account for everything still pending as no-longer-in-flight.
                shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                drop(inbound_tx);
                return;
            }
            if dir_actor.stalled.load(Ordering::SeqCst) {
                continue;
            }

            // Deliver every item whose deadline has now passed, in FIFO order.
            let now = tokio::time::Instant::now();
            while let Some((deadline, _)) = pending.front() {
                if *deadline > now {
                    break;
                }
                let (_, bytes) = pending.pop_front().unwrap();
                if dir_actor.drop_on_overflow.load(Ordering::SeqCst) {
                    match inbound_tx.try_send(bytes) {
                        Ok(()) => {
                            shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            // Buffer full → drop the subscriber (cut).
                            dir_actor.cut.store(true, Ordering::SeqCst);
                            shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                            // Remaining pending no longer in flight.
                            shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                            drop(inbound_tx);
                            return;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                            shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                            return;
                        }
                    }
                } else {
                    // Backpressure: await buffer space (models TCP flow-control).
                    if inbound_tx.send(bytes).await.is_err() {
                        shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                        shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                        return;
                    }
                    shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                    // A fault may have flipped while we awaited buffer space.
                    if dir_actor.cut.load(Ordering::SeqCst) {
                        shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                        drop(inbound_tx);
                        return;
                    }
                }
            }
        }
    });

    Wire {
        staging_tx,
        inbound_rx,
        dir,
    }
}

struct SimListener {
    local: SocketAddr,
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<Box<dyn ReplicationConnection>>>,
    shared: Arc<SimShared>,
}

#[async_trait]
impl ReplicationListener for SimListener {
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>> {
        self.incoming.lock().await.recv().await
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for SimListener {
    fn drop(&mut self) {
        self.shared.routing.lock().unwrap().remove(&self.local);
    }
}

struct SimConnection {
    local: SocketAddr,
    peer: SocketAddr,
    /// Outbound staging: `send` pushes encoded bytes (ordered).
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// Outbound direction state — `send` fails fast once it is cut.
    out_dir: Arc<DirState>,
    /// Inbound decoded-frame source.
    inbound: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    /// Inbound direction state (for completeness / future introspection).
    #[allow(dead_code)]
    in_dir: Arc<DirState>,
}

#[async_trait]
impl ReplicationConnection for SimConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        if self.out_dir.cut.load(Ordering::SeqCst) {
            return Err(SendError::Closed);
        }
        // The codec plays through every path: encode to bytes, move bytes.
        let bytes = encode_frame(&frame);
        self.out.send(bytes).map_err(|_| SendError::Closed)
    }

    async fn recv(&self) -> Option<Frame> {
        let bytes = self.inbound.lock().await.recv().await?;
        // Decode back from bytes — sim moves encoded `Vec<u8>`, never `Frame`.
        // A decode failure here is a codec/test bug, not a peer condition;
        // surface it as a clean close rather than panicking the actor.
        decode_frame(&bytes).ok()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::time::Duration;

    use sip_clock::testkit::advance_in_100ms_chunks;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn data_frame(counter: u64, body: &[u8]) -> Frame {
        Frame::Data {
            at: crate::Watermark::new(1, counter),
            op: crate::Op::Update,
            partition: crate::Partition::Bak,
            call_ref: format!("p|call{counter}|tag"),
            call_gen: 7,
            body_ttl_ms: 1000,
            indexes: vec!["idx".into()],
            body: Some(StdArc::from(body)),
        }
    }

    fn noop(counter: u64) -> Frame {
        Frame::Noop {
            at: crate::Watermark::new(1, counter),
        }
    }

    /// Open a connected (client, server) pair from A to B's listener.
    async fn connected_pair(
        net: &SimulatedReplicationNetwork,
        a: SocketAddr,
        b: SocketAddr,
    ) -> (
        Box<dyn ReplicationConnection>,
        Box<dyn ReplicationConnection>,
        Box<dyn ReplicationListener>,
    ) {
        let listener = net.listen(b).await.unwrap();
        let client = net.connect_from(a, b).await.unwrap();
        let server = listener.accept().await.unwrap();
        (client, server, listener)
    }

    #[tokio::test(start_paused = true)]
    async fn connect_send_recv_happy_path_bidirectional() {
        let net = SimulatedReplicationNetwork::with_delay(5);
        let (client, server, _l) = connected_pair(&net, addr(1000), addr(2000)).await;

        // A → B
        let f = noop(1);
        client.send(f.clone()).await.unwrap();
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(server.recv().await, Some(f));

        // B → A (bidirectional, identical frame round-trips through bytes)
        let g = data_frame(2, b"hello-body");
        server.send(g.clone()).await.unwrap();
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(client.recv().await, Some(g));
    }

    #[tokio::test(start_paused = true)]
    async fn ordering_preserved_for_n_frames() {
        let net = SimulatedReplicationNetwork::with_delay(3);
        let (client, server, _l) = connected_pair(&net, addr(1001), addr(2001)).await;

        for i in 0..10 {
            client.send(noop(i)).await.unwrap();
        }
        advance_in_100ms_chunks(Duration::from_millis(50)).await;
        for i in 0..10 {
            assert_eq!(server.recv().await, Some(noop(i)));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transit_delay_blocks_until_elapsed_and_zero_is_coerced() {
        // Request 0 → coerced to >= 1 ms: NOT delivered instantly.
        let net = SimulatedReplicationNetwork::with_delay(0);
        let (client, server, _l) = connected_pair(&net, addr(1002), addr(2002)).await;

        client.send(noop(1)).await.unwrap();

        // Nothing before the (coerced 1 ms) delay elapses.
        let early = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
        assert!(early.is_err(), "delivered before transit delay (0 not coerced?)");

        advance_in_100ms_chunks(Duration::from_millis(2)).await;
        assert_eq!(server.recv().await, Some(noop(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn transit_delay_explicit_window() {
        let net = SimulatedReplicationNetwork::with_delay(50);
        let (client, server, _l) = connected_pair(&net, addr(1003), addr(2003)).await;
        client.send(noop(1)).await.unwrap();

        advance_in_100ms_chunks(Duration::from_millis(40)).await;
        let before = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
        assert!(before.is_err(), "delivered before 50ms");

        advance_in_100ms_chunks(Duration::from_millis(20)).await;
        assert_eq!(server.recv().await, Some(noop(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn cut_yields_none_and_send_fails() {
        let net = SimulatedReplicationNetwork::with_delay(5);
        let (a, b) = (addr(1004), addr(2004));
        let (client, server, _l) = connected_pair(&net, a, b).await;

        // Cut both directions of this pair.
        net.apply_fault(Fault::Cut { src: a, dst: b });
        net.apply_fault(Fault::Cut { src: b, dst: a });
        advance_in_100ms_chunks(Duration::from_millis(10)).await;

        assert_eq!(server.recv().await, None, "recv should yield None after cut");
        assert_eq!(client.recv().await, None);
        // Future send fails.
        let r = client.send(noop(1)).await;
        assert!(matches!(r, Err(SendError::Closed)));
    }

    #[tokio::test(start_paused = true)]
    async fn stall_then_resume_delivers_buffered_in_order() {
        let net = SimulatedReplicationNetwork::with_delay(5);
        let (a, b) = (addr(1005), addr(2005));
        let (client, server, _l) = connected_pair(&net, a, b).await;

        net.apply_fault(Fault::Stall { src: a, dst: b });
        for i in 0..5 {
            client.send(noop(i)).await.unwrap();
        }
        advance_in_100ms_chunks(Duration::from_millis(50)).await;
        // Stalled: nothing delivered.
        let blocked = tokio::time::timeout(Duration::from_micros(1), server.recv()).await;
        assert!(blocked.is_err(), "stall leaked a frame");

        net.apply_fault(Fault::Resume { src: a, dst: b });
        advance_in_100ms_chunks(Duration::from_millis(50)).await;
        for i in 0..5 {
            assert_eq!(server.recv().await, Some(noop(i)), "out of order after resume");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn partition_cuts_both_then_heal_allows_reconnect() {
        let net = SimulatedReplicationNetwork::with_delay(5);
        let (a, b) = (addr(1006), addr(2006));
        let (client, server, listener) = connected_pair(&net, a, b).await;

        net.apply_fault(Fault::Partition { a, b });
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(client.recv().await, None);
        assert_eq!(server.recv().await, None);

        // New connect blocked while partitioned.
        let blocked = net.connect_from(a, b).await;
        assert!(matches!(blocked, Err(ConnectError::Blocked { .. })));

        // Heal → reconnect succeeds (fresh client/server pair).
        net.apply_fault(Fault::Heal { a, b });
        let client2 = net.connect_from(a, b).await.unwrap();
        let server2 = listener.accept().await.unwrap();
        client2.send(noop(99)).await.unwrap();
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(server2.recv().await, Some(noop(99)));
    }

    #[tokio::test(start_paused = true)]
    async fn buffer_overflow_drop_subscriber() {
        // Tiny buffer, drop-on-overflow armed, no drainer → cut.
        let net = SimulatedReplicationNetwork::new(5, 2);
        let (a, b) = (addr(1007), addr(2007));
        net.apply_fault(Fault::DropOnOverflow { src: a, dst: b });
        let (client, server, _l) = connected_pair(&net, a, b).await;

        // Send more than the buffer holds; never drain `server`, so the bounded
        // buffer fills and the actor drops the subscriber (cuts). Crucially we
        // do NOT recv during the advance — a concurrent drain would keep pace
        // and the buffer would never overflow ("drive the protocol between
        // advances" hazard).
        for i in 0..10 {
            // send itself never blocks (unbounded staging); delivery actor cuts.
            let _ = client.send(noop(i)).await;
        }
        // Let the actor fill the buffer (cap 2) and trip the overflow → cut.
        // Advance past all transit deadlines, then yield (without recv) so the
        // woken actor runs to completion: it fills the bounded buffer and, on
        // the first frame that does not fit, drops the subscriber. Crucially we
        // do NOT recv here — a concurrent drain would free slots and the buffer
        // would never overflow.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        advance_in_100ms_chunks(Duration::from_millis(100)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }

        // The subscriber was dropped: the outbound (c2s) direction is cut, so a
        // further send fails — the load-bearing signal of the overflow drop.
        assert!(
            matches!(client.send(noop(100)).await, Err(SendError::Closed)),
            "drop-on-overflow should have cut the subscriber",
        );

        // Draining the bounded prefix terminates in `None`, and far fewer than
        // the 10 sent frames buffered (proving the buffer was bounded).
        let mut drained = 0;
        while server.recv().await.is_some() {
            drained += 1;
            assert!(drained < 10, "buffer was not bounded; drained {drained}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn backpressure_does_not_lose_ordering_with_tiny_buffer() {
        // Default (await) backpressure: tiny buffer, drain slowly, all frames
        // arrive in order, none lost.
        let net = SimulatedReplicationNetwork::new(3, 2);
        let (client, server, _l) = connected_pair(&net, addr(1008), addr(2008)).await;

        for i in 0..6 {
            client.send(noop(i)).await.unwrap();
        }
        // Drain one at a time, advancing between each so the parked delivery
        // actor wakes and refills the bounded buffer.
        for i in 0..6 {
            advance_in_100ms_chunks(Duration::from_millis(10)).await;
            assert_eq!(server.recv().await, Some(noop(i)));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn connect_refused_when_no_listener() {
        let net = SimulatedReplicationNetwork::with_delay(5);
        let r = net.connect_from(addr(1009), addr(2009)).await;
        assert!(matches!(r, Err(ConnectError::Refused(_))));
    }
}
