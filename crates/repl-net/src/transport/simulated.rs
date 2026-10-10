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
//! the partition is cleared. Three disconnect flavours are modelled so a test
//! can cover every case:
//! - **clean cut** ([`Fault::Cut`]) — immediate close: `recv`→`None`,
//!   `send`→`Closed` (a graceful FIN/RST).
//! - **black-hole / hung** ([`Fault::Block`]) — the peer stops pulling, so
//!   `send` BLOCKS on a full in-flight window with no error and no close
//!   (a half-open peer that never sends a reset). Cleared by [`Fault::Resume`].
//! - **error after a delay** ([`Fault::ErrorAfter`]) — after `ms`, `send`
//!   returns [`SendError::Io`], `recv`→`None`, and a fresh `connect_from` is
//!   rejected with [`ConnectError::Io`] (a reset some time into the connection).
//!
//! ## Node faults: a directed fault named on two declared endpoints
//! A puller opens its stream from an ephemeral local, so the live wires of a
//! connection run between that local and a listener, never between two listen
//! addresses. The harness therefore declares each node's listen address
//! ([`SimulatedReplicationNetwork::declare_endpoint`]); the server end reads
//! the `caller` of the opening `PullRequest` and attributes the client local to
//! the node that owns it (`owner`). A `Delay`/`Stall`/`Resume`/`Cut` whose `src`
//! and `dst` are both declared lands in a **node-fault overlay** keyed by the
//! directed owner pair, which every wire actor reads through `owner` at the
//! three sites it decides at (staging delay, hold, death): a stream opened
//! after the fault, or attributed after it, inherits it by construction.
//! `Partition`/`Heal` work the same way (owner compare at delivery + connect).
//! A pair with an undeclared end keeps the per-direction semantics above.
//!
//! A node connects through its own handle ([`NodeReplicationNetwork`], from
//! [`SimulatedReplicationNetwork::as_node`]) whenever the harness can hand it
//! one: the stream is then attributed at `connect`, so a partition refuses
//! the connect outright — the puller never reads a peer it cannot reach as
//! reached. The `PullRequest` attribution covers a stream opened through the
//! bare fabric.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
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
/// shared, or applied to a live fabric. `delay`/`stall`/`cut` are **directed**
/// (apply to `src → dst`); `partition`/`heal` are **bidirectional** (both
/// directions between two endpoints). `reconnect` is not a fault — it is just
/// a fresh `connect` succeeding once any `cut`/partition on the pair is cleared.
///
/// A directed fault whose `src` and `dst` are both **declared endpoints**
/// ([`declare_endpoint`](SimulatedReplicationNetwork::declare_endpoint)) is a
/// node fault: it applies to every stream between the two nodes, present and
/// future, in that direction — the streams either node's puller opened from an
/// ephemeral local included. Any other pair is one wire direction, literally.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the transit delay `src → dst` (coerced to `>= 1 ms`). A frame
    /// stamped after the fault (the delivery actor stamps at its next poll)
    /// waits the new delay; one already stamped keeps its deadline.
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
    /// that direction fail; their `recv` yields `None`. Between declared
    /// endpoints the cut outlives the streams it closes — a stream reopened
    /// under it dies as soon as it is attributed — until a [`Fault::Heal`].
    Cut {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Partition two endpoints: hold delivery in **both** directions (frames
    /// buffer in order and flush on [`Fault::Heal`]) and refuse new `connect`s
    /// between them until then.
    Partition {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Heal a partition: clear the block so a fresh `connect` succeeds, and
    /// clear every node fault between the two endpoints, both directions.
    Heal {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Arm buffer-overflow → drop-subscriber on `src → dst`: when the bounded
    /// inbound buffer is full, the delivery actor cuts the connection instead
    /// of awaiting space (the "buffer-full → drop subscriber → reconnect"
    /// scenario).
    DropOnOverflow {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// **Black-hole** `src → dst`: the peer stops pulling, so the application
    /// `send` BLOCKS once the in-flight window is full — modelling a TCP sender
    /// stuck on a full socket buffer with a dead reader. No error, no close:
    /// `send` simply never completes until the peer drains (its `recv` releases a
    /// window slot), the block is cleared by [`Fault::Resume`], or the direction
    /// is later [`Cut`](Fault::Cut)/[`ErrorAfter`](Fault::ErrorAfter). This is the
    /// half-open / hung-peer case that a clean `Cut` (immediate close) does not
    /// cover.
    Block {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// **Network error after a delay** on `src → dst`: after `ms` elapse, an
    /// established `send` returns [`SendError::Io`], the delivery actor tears the
    /// inbound down (`recv` → `None`), and a fresh [`connect_from`] on this pair
    /// is rejected with [`ConnectError::Io`] — modelling a reset (ECONNRESET)
    /// some time into the connection's life, distinct from a clean `Cut`.
    ///
    /// [`connect_from`]: SimulatedReplicationNetwork::connect_from
    ErrorAfter {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
        /// Delay before the error fires, in milliseconds.
        ms: u64,
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
    /// Flow-control black-hole ([`Fault::Block`]): the application `send` blocks
    /// once `inflight` reaches `cap`. Cleared by [`Fault::Resume`].
    block: AtomicBool,
    /// Frames sent on this direction but not yet consumed by the peer's `recv` —
    /// the flow-control window used by `block`. Always tracked; only gates `send`
    /// while `block` is set.
    inflight: AtomicUsize,
    /// Window capacity (= the inbound buffer cap) for the `block` flow control.
    cap: usize,
    /// Simulated network error ([`Fault::ErrorAfter`]) has fired: `send` returns
    /// `Io`, the delivery actor tears the inbound down (`recv` → `None`).
    errored: AtomicBool,
    /// Absolute deadline at which a fresh `connect_from` on this pair is rejected
    /// with `ConnectError::Io` ([`Fault::ErrorAfter`]).
    error_at: Mutex<Option<tokio::time::Instant>>,
    /// Woken when any of the above flip (or a window slot frees), so a
    /// stalled/parked actor — or a `block`ed writer — re-checks.
    wake: Notify,
}

impl DirState {
    fn new(delay_ms: u64, drop_on_overflow: bool, cap: usize) -> Self {
        Self {
            delay_ms: AtomicU64::new(delay_ms.max(1)),
            stalled: AtomicBool::new(false),
            cut: AtomicBool::new(false),
            drop_on_overflow: AtomicBool::new(drop_on_overflow),
            block: AtomicBool::new(false),
            inflight: AtomicUsize::new(0),
            cap: cap.max(1),
            errored: AtomicBool::new(false),
            error_at: Mutex::new(None),
            wake: Notify::new(),
        }
    }

    /// A terminal condition that should fail/short-circuit `send` and the actor.
    fn is_dead(&self) -> bool {
        self.cut.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst)
    }
}

/// A node fault between two declared endpoints, read by every wire actor whose
/// owner pair matches. `delay_ms` combines with the wire's own delay by `max`.
#[derive(Clone, Copy, Debug, Default)]
struct NodeFault {
    /// Transit delay in ms (`>= 1`) when set.
    delay_ms: Option<u64>,
    /// Delivery held; buffered in order until a `Resume` or `Heal`.
    stalled: bool,
    /// Every wire in the direction dies; a reopened one dies once attributed.
    cut: bool,
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
    /// Enforced on the OWNER of each end, so a stream a puller opens from an
    /// ephemeral local is cut with its node, new streams included.
    partitions: Mutex<HashSet<Pair>>,
    /// Node ordinal → its listen address, declared by the harness. The bridge
    /// from a `PullRequest`'s `caller` name to an address the fault tables key on.
    endpoints: Mutex<HashMap<String, SocketAddr>>,
    /// Client-side ephemeral address → the listen address of the node whose
    /// puller opened the stream, learned from the `caller` on its first
    /// `PullRequest`.
    stream_owner: Mutex<HashMap<SocketAddr, SocketAddr>>,
    /// Directed faults between two declared endpoints, keyed by the owner pair;
    /// read by every wire through its [`WireView`], so they reach streams that
    /// run between an ephemeral local and a listener.
    node_faults: Mutex<HashMap<Pair, NodeFault>>,
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
                let drop_on = self.drop_on_overflow_pairs.lock().unwrap().contains(&(src, dst));
                Arc::new(DirState::new(self.default_delay_ms, drop_on, self.buffer_cap))
            })
            .clone()
    }

    /// The node an address belongs to: a declared listen address is its own
    /// owner, a client ephemeral is the node whose puller opened the stream, and
    /// an unattributed address stands for itself.
    fn owner(&self, addr: SocketAddr) -> SocketAddr {
        self.stream_owner.lock().unwrap().get(&addr).copied().unwrap_or(addr)
    }

    /// Attribute the stream opened from `client` to the node `caller` names,
    /// returning that node's address when `caller` is declared.
    fn attribute(&self, client: SocketAddr, caller: &str) -> Option<SocketAddr> {
        let owner = self.endpoints.lock().unwrap().get(caller).copied()?;
        self.stream_owner.lock().unwrap().insert(client, owner);
        Some(owner)
    }

    /// Forget a closed stream's attribution. [`synth_local`] draws its ephemeral
    /// ports from a wrapping counter, so an entry left behind would hand a later
    /// stream the owner of a long-dead one — and with it that node's partitions.
    /// The stream's own wires keep the owner they resolved ([`WireView`]), so
    /// what they still hold stays held. A listen address is never a key here,
    /// so dropping the server end is a no-op.
    fn forget_stream(&self, client: SocketAddr) {
        self.stream_owner.lock().unwrap().remove(&client);
    }

    /// Is `addr` a declared listen address — one a node fault can be named on?
    fn is_declared(&self, addr: SocketAddr) -> bool {
        self.endpoints.lock().unwrap().values().any(|a| *a == addr)
    }

    /// Is `addr` a listener's own address — live on the routing table or
    /// declared — rather than a client end's ephemeral local?
    fn is_listener(&self, addr: SocketAddr) -> bool {
        self.routing.lock().unwrap().contains_key(&addr) || self.is_declared(addr)
    }

    /// The node fault on a resolved owner pair ([`WireView::owners`]).
    fn node_fault_on(&self, owners: Pair) -> NodeFault {
        self.node_faults.lock().unwrap().get(&owners).copied().unwrap_or_default()
    }

    /// Edit the node fault on the owner pair `(src, dst)` and wake every wire,
    /// so an actor parked on a stale read re-reads at once.
    fn edit_node_fault(&self, src: SocketAddr, dst: SocketAddr, f: impl FnOnce(&mut NodeFault)) {
        f(self.node_faults.lock().unwrap().entry((src, dst)).or_default());
        self.wake_all();
    }

    /// Is `src → dst` partitioned, reading each end as its owning node?
    fn partitioned(&self, src: SocketAddr, dst: SocketAddr) -> bool {
        self.partitioned_on((src, dst), (self.owner(src), self.owner(dst)))
    }

    /// Is the wire `wire` partitioned, literally or on its resolved owner pair?
    fn partitioned_on(&self, wire: Pair, owners: Pair) -> bool {
        let p = self.partitions.lock().unwrap();
        p.contains(&wire) || p.contains(&owners)
    }

    /// Wake every live direction, so a partition change is re-read at once.
    fn wake_all(&self) {
        for d in self.dir_state.lock().unwrap().values() {
            d.wake.notify_waiters();
        }
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
                endpoints: Mutex::new(HashMap::new()),
                stream_owner: Mutex::new(HashMap::new()),
                node_faults: Mutex::new(HashMap::new()),
                in_flight: AtomicI64::new(0),
            }),
        }
    }

    /// Convenience: a fabric with the default 8-frame buffer.
    pub fn with_delay(transit_delay_ms: u64) -> Self {
        Self::new(transit_delay_ms, 8)
    }

    /// Install one fault. A directed fault between two declared endpoints
    /// lands in the node-fault overlay (see the module docs); any other
    /// directed fault edits its wire direction, pre-seeding the state of a
    /// not-yet-connected one so the next `connect` inherits it.
    ///
    /// May be called before or after the fabric is shared — all state is behind
    /// interior mutability, so a fault flips the **live** actor's flags.
    pub fn apply_fault(&self, fault: Fault) {
        let sh = &self.shared;
        let node_pair = |src, dst| sh.is_declared(src) && sh.is_declared(dst);
        match fault {
            Fault::Delay { src, dst, ms } if node_pair(src, dst) => {
                sh.edit_node_fault(src, dst, |n| n.delay_ms = Some(ms.max(1)));
            }
            Fault::Delay { src, dst, ms } => {
                sh.dir(src, dst).delay_ms.store(ms.max(1), Ordering::SeqCst);
            }
            Fault::Stall { src, dst } if node_pair(src, dst) => {
                sh.edit_node_fault(src, dst, |n| n.stalled = true);
            }
            Fault::Stall { src, dst } => {
                sh.dir(src, dst).stalled.store(true, Ordering::SeqCst);
            }
            Fault::Resume { src, dst } => {
                // Resume clears any pause on this direction: a `Stall` (delivery
                // hold, node or wire) and a `Block` (writer backpressure
                // black-hole, wire only) alike.
                if node_pair(src, dst) {
                    sh.edit_node_fault(src, dst, |n| n.stalled = false);
                } else {
                    let d = sh.dir(src, dst);
                    d.stalled.store(false, Ordering::SeqCst);
                    d.block.store(false, Ordering::SeqCst);
                    d.wake.notify_waiters();
                }
            }
            Fault::Cut { src, dst } if node_pair(src, dst) => {
                sh.edit_node_fault(src, dst, |n| n.cut = true);
            }
            Fault::Cut { src, dst } => {
                let d = sh.dir(src, dst);
                d.cut.store(true, Ordering::SeqCst);
                d.wake.notify_waiters();
            }
            Fault::Partition { a, b } => {
                {
                    let mut p = sh.partitions.lock().unwrap();
                    p.insert((a, b));
                    p.insert((b, a));
                }
                for (s, d) in [(a, b), (b, a)] {
                    let st = sh.dir(s, d);
                    st.cut.store(true, Ordering::SeqCst);
                    st.wake.notify_waiters();
                }
                // Streams the two nodes' pullers already opened run between
                // ephemeral locals, not the listen pair: they are held at
                // delivery on the owner compare, so wake them to re-read it.
                sh.wake_all();
            }
            Fault::Heal { a, b } => {
                {
                    let mut p = sh.partitions.lock().unwrap();
                    p.remove(&(a, b));
                    p.remove(&(b, a));
                    // The cut DirStates stay cut (existing conns are dead); a fresh
                    // connect creates new DirStates. Drop the stale ones so the new
                    // direction starts clean.
                    let mut ds = sh.dir_state.lock().unwrap();
                    ds.remove(&(a, b));
                    ds.remove(&(b, a));
                    // Every node fault between the two, both directions.
                    let mut nf = sh.node_faults.lock().unwrap();
                    nf.remove(&(a, b));
                    nf.remove(&(b, a));
                }
                // Held streams between the two nodes flush now, in order.
                sh.wake_all();
            }
            Fault::DropOnOverflow { src, dst } => {
                sh.drop_on_overflow_pairs.lock().unwrap().insert((src, dst));
                sh.dir(src, dst).drop_on_overflow.store(true, Ordering::SeqCst);
            }
            Fault::Block { src, dst } => {
                // Arm the flow-control black-hole. No wake needed: arming only
                // makes a *future* send park once the window fills.
                sh.dir(src, dst).block.store(true, Ordering::SeqCst);
            }
            Fault::ErrorAfter { src, dst, ms } => {
                let d = sh.dir(src, dst);
                let at = tokio::time::Instant::now() + Duration::from_millis(ms);
                *d.error_at.lock().unwrap() = Some(at);
                // Drive the error on the live (established) connection: after the
                // delay, flip `errored` and wake the actor so it tears the inbound
                // down (recv → None) even if the connection is otherwise idle.
                let d2 = d.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(at).await;
                    d2.errored.store(true, Ordering::SeqCst);
                    d2.wake.notify_waiters();
                });
            }
        }
    }

    /// Builder form of [`apply_fault`](Self::apply_fault).
    pub fn with_fault(self, fault: Fault) -> Self {
        self.apply_fault(fault);
        self
    }

    /// `ordinal`'s own handle on this fabric: every stream it opens is
    /// attributed to it at `connect` (see [`NodeReplicationNetwork`]).
    pub fn as_node(&self, ordinal: &str) -> NodeReplicationNetwork {
        NodeReplicationNetwork { shared: self.shared.clone(), ordinal: ordinal.to_string() }
    }

    /// Declare `ordinal`'s replication listen address, so a stream its puller
    /// opens from an ephemeral local is attributed to it (the `caller` on the
    /// opening `PullRequest` names the ordinal) and a directed fault named on
    /// two declared addresses is a node fault (see [`Fault`]). Without a
    /// declaration a stream stands for its own address and a fault between two
    /// listen addresses reaches only the wires that literally run on the pair.
    /// Declaring an ordinal again re-points it: later attributions and faults
    /// read the new address.
    pub fn declare_endpoint(&self, ordinal: &str, listen: SocketAddr) {
        self.shared.endpoints.lock().unwrap().insert(ordinal.to_string(), listen);
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

    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
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
        // Partitioned? refuse — reading each end as its owning node, so a
        // reconnect from an already-attributed local is refused too.
        if self.shared.partitioned(local, dst) {
            return Err(ConnectError::Blocked { addr: dst, reason: "partitioned".into() });
        }

        // Network error armed on this pair? Reject the connect — immediately if it
        // has already fired, else after the remaining `ErrorAfter` delay (models a
        // connect that resets / errors out rather than being cleanly refused).
        {
            let d = self.shared.dir(local, dst);
            if d.errored.load(Ordering::SeqCst) {
                return Err(ConnectError::Io("simulated network error".into()));
            }
            let at = *d.error_at.lock().unwrap();
            if let Some(at) = at {
                tokio::time::sleep_until(at).await;
                return Err(ConnectError::Io("simulated network error".into()));
            }
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
            shared: self.shared.clone(),
            out: c2s.staging_tx,
            out_dir: c2s.dir.clone(),
            out_view: c2s.view.clone(),
            inbound: tokio::sync::Mutex::new(s2c.inbound_rx),
            in_dir: s2c.dir.clone(),
            in_view: s2c.view.clone(),
        };
        let server = SimConnection {
            local: dst,
            peer: local,
            shared: self.shared.clone(),
            out: s2c.staging_tx,
            out_dir: s2c.dir,
            out_view: s2c.view,
            inbound: tokio::sync::Mutex::new(c2s.inbound_rx),
            in_dir: c2s.dir,
            in_view: c2s.view,
        };

        // Hand the server end to the listener's accept queue. If the receiver
        // is gone (listener dropped between lookup and now) the connect still
        // "succeeds" but the server end is discarded — its drop closes the
        // wire and the client's next recv yields None.
        let _ = incoming_tx.send(Box::new(server) as Box<dyn ReplicationConnection>);

        Ok(Box::new(client))
    }
}

/// One node's handle on a [`SimulatedReplicationNetwork`]: `connect` draws the
/// ephemeral local and attributes it to the node's declared listen address
/// before the fabric decides, so a [`Fault::Partition`] on the node refuses the
/// connect with [`ConnectError::Blocked`] — a peer behind a partition is never
/// reached — and a node [`Fault::Cut`] closes the stream at once. An ordinal
/// with no declaration connects as the bare fabric does. `listen` is the
/// fabric's own.
#[derive(Clone)]
pub struct NodeReplicationNetwork {
    shared: Arc<SimShared>,
    ordinal: String,
}

#[async_trait]
impl ReplicationNetwork for NodeReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let local = synth_local(dst);
        let owner = self.shared.endpoints.lock().unwrap().get(&self.ordinal).copied();
        if let Some(owner) = owner {
            self.shared.stream_owner.lock().unwrap().insert(local, owner);
        }
        let fabric = SimulatedReplicationNetwork { shared: self.shared.clone() };
        let conn = fabric.connect_from(local, dst).await;
        if conn.is_err() {
            self.shared.forget_stream(local);
        }
        conn
    }

    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
        SimulatedReplicationNetwork { shared: self.shared.clone() }.listen(local).await
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

/// One wire's view of the owner pair it is judged on, shared by its delivery
/// actor and the connection handle that sends on it, so the send-side and
/// the delivery-side verdicts read one pair. Each end resolves to its owning
/// node once — at spawn when the fabric already knows it (a node handle's
/// connect), or when the opening `PullRequest` names it ([`learn`]) — and the
/// snapshot then outlives the stream's attribution: a handle dropped mid-hold
/// (a crash) forgets its ephemeral local without releasing the frames the wire
/// still holds for it.
///
/// [`learn`]: WireView::learn
struct WireView {
    shared: Arc<SimShared>,
    wire: Pair,
    owners: Mutex<Pair>,
}

impl WireView {
    fn new(shared: Arc<SimShared>, src: SocketAddr, dst: SocketAddr) -> Self {
        let owners = (shared.owner(src), shared.owner(dst));
        Self { shared, wire: (src, dst), owners: Mutex::new(owners) }
    }

    /// The end `addr` of this wire belongs to `owner`: set it if that end still
    /// stands for itself.
    fn learn(&self, addr: SocketAddr, owner: SocketAddr) {
        let mut owners = self.owners.lock().unwrap();
        if self.wire.0 == addr && owners.0 == addr {
            owners.0 = owner;
        }
        if self.wire.1 == addr && owners.1 == addr {
            owners.1 = owner;
        }
    }

    /// The owner pair, re-reading an end only while it still stands for itself.
    fn owners(&self) -> Pair {
        let (src, dst) = self.wire;
        let mut owners = self.owners.lock().unwrap();
        if owners.0 == src {
            owners.0 = self.shared.owner(src);
        }
        if owners.1 == dst {
            owners.1 = self.shared.owner(dst);
        }
        *owners
    }

    fn node_fault(&self) -> NodeFault {
        self.shared.node_fault_on(self.owners())
    }

    fn partitioned(&self) -> bool {
        self.shared.partitioned_on(self.wire, self.owners())
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
    /// The owner pair the direction is judged on.
    view: Arc<WireView>,
}

/// Spawn the per-direction delivery actor for `src → dst` and return its
/// staging sender + inbound receiver + shared fault state.
fn spawn_wire(shared: Arc<SimShared>, src: SocketAddr, dst: SocketAddr) -> Wire {
    let dir = shared.dir(src, dst);
    let view = Arc::new(WireView::new(shared.clone(), src, dst));
    let (staging_tx, mut staging_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (inbound_tx, inbound_rx) = mpsc::channel::<Vec<u8>>(shared.buffer_cap);

    let dir_actor = dir.clone();
    let view_actor = view.clone();
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
        // Items carry their deadline, stamped at the poll that takes them off
        // the staging queue; a `Delay` fault governs the items stamped after
        // it. The delay, the hold and the death are each read at their site —
        // the wire's own state and the node fault on the pair the view
        // resolved once — so a fault applied between a `send` and this poll
        // governs that frame.
        let mut pending: VecDeque<(tokio::time::Instant, Vec<u8>)> = VecDeque::new();
        let mut last_deadline = tokio::time::Instant::now();
        let view = view_actor;
        let delay_now = || {
            let wire = dir_actor.delay_ms.load(Ordering::SeqCst);
            let node = view.node_fault().delay_ms.unwrap_or(0);
            Duration::from_millis(wire.max(node).max(1))
        };
        let dead = || dir_actor.is_dead() || view.node_fault().cut;
        let held = || {
            dir_actor.stalled.load(Ordering::SeqCst)
                || view.node_fault().stalled
                || view.partitioned()
        };

        loop {
            if dead() {
                // Everything still pending is no longer in flight.
                shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                drop(inbound_tx); // peer recv → None (cut or simulated error)
                return;
            }

            // Pull every currently-staged item, stamping monotonic deadlines so
            // a burst of sends becomes a run of ordered timers.
            loop {
                match staging_rx.try_recv() {
                    Ok(bytes) => {
                        let now = tokio::time::Instant::now();
                        let base = last_deadline.max(now);
                        let deadline = base + delay_now();
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
                            let deadline = last_deadline.max(now) + delay_now();
                            last_deadline = deadline;
                            pending.push_back((deadline, bytes));
                            shared_actor.in_flight.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        None => { drop(inbound_tx); return; }
                    }
                }
            }

            // Stalled (wire or node), or partitioned by owner: hold everything
            // until resumed / healed / cut. A held direction buffers in order
            // and flushes whole.
            if held() {
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

            if dead() {
                // Account for everything still pending as no-longer-in-flight.
                shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                drop(inbound_tx);
                return;
            }
            if held() {
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
                            shared_actor
                                .in_flight
                                .fetch_sub(pending.len() as i64, Ordering::Relaxed);
                            drop(inbound_tx);
                            return;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            shared_actor.in_flight.fetch_sub(1, Ordering::Relaxed);
                            shared_actor
                                .in_flight
                                .fetch_sub(pending.len() as i64, Ordering::Relaxed);
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
                    if dead() {
                        shared_actor.in_flight.fetch_sub(pending.len() as i64, Ordering::Relaxed);
                        drop(inbound_tx);
                        return;
                    }
                }
            }
        }
    });

    Wire { staging_tx, inbound_rx, dir, view }
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
    /// The fabric, for the owner attribution a `PullRequest` carries.
    shared: Arc<SimShared>,
    /// Outbound staging: `send` pushes encoded bytes (ordered).
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// Outbound direction state — `send` fails fast once it is cut.
    out_dir: Arc<DirState>,
    /// The outbound wire's owner pair — `send` fails fast once it is cut.
    out_view: Arc<WireView>,
    /// Inbound decoded-frame source.
    inbound: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    /// Inbound direction state — `recv` releases its flow-control window slot
    /// (waking a `block`ed writer) on this direction.
    in_dir: Arc<DirState>,
    /// The inbound wire's owner pair, taught this end's owner with `out_view`.
    in_view: Arc<WireView>,
}

#[async_trait]
impl ReplicationConnection for SimConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        // A puller names itself on its opening `PullRequest`: that is what ties
        // this stream's ephemeral local to the node that owns it — on the
        // fabric's map and on both of the stream's wires, before either actor
        // judges a frame on it. Only a client end is attributed: a listener's
        // address is its own owner.
        if let Frame::PullRequest { caller, .. } = &frame {
            if !self.shared.is_listener(self.local) {
                if let Some(owner) = self.shared.attribute(self.local, caller) {
                    self.out_view.learn(self.local, owner);
                    self.in_view.learn(self.local, owner);
                }
            }
        }
        // A simulated network error wins over a clean cut; a cut on the wire
        // and a cut on the owner pair fail the send alike.
        if self.out_dir.errored.load(Ordering::SeqCst) {
            return Err(SendError::Io("simulated network error".into()));
        }
        if self.out_dir.cut.load(Ordering::SeqCst) || self.out_view.node_fault().cut {
            return Err(SendError::Closed);
        }

        // Flow-control black-hole ([`Fault::Block`]): when the peer is not pulling
        // and the in-flight window is full, the write BLOCKS on buffer space —
        // modelling a TCP sender stuck on a full socket buffer with a dead reader.
        // It unblocks when the peer drains (recv releases a slot), the block is
        // cleared (Resume), or the direction is cut/errored. Arming the `Notified`
        // future before the window check avoids a lost wakeup vs `recv`'s notify.
        while self.out_dir.block.load(Ordering::SeqCst) {
            if self.out_dir.errored.load(Ordering::SeqCst) {
                return Err(SendError::Io("simulated network error".into()));
            }
            if self.out_dir.cut.load(Ordering::SeqCst) {
                return Err(SendError::Closed);
            }
            let notified = self.out_dir.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.out_dir.inflight.load(Ordering::SeqCst) < self.out_dir.cap {
                break;
            }
            notified.await;
        }

        // The codec plays through every path: encode to bytes, move bytes.
        let bytes = encode_frame(&frame);
        // Account the frame against the flow-control window (released by the
        // peer's `recv`). Always tracked so `block` can be armed mid-stream.
        self.out_dir.inflight.fetch_add(1, Ordering::SeqCst);
        self.out.send(bytes).map_err(|_| SendError::Closed)
    }

    async fn recv(&self) -> Option<Frame> {
        let bytes = self.inbound.lock().await.recv().await?;
        // Release one flow-control window slot for the sender + wake a blocked
        // writer (`in_dir` == the sender's `out_dir` for this direction).
        if self.in_dir.inflight.load(Ordering::SeqCst) > 0 {
            self.in_dir.inflight.fetch_sub(1, Ordering::SeqCst);
        }
        self.in_dir.wake.notify_waiters();
        // Decode back from bytes — sim moves encoded `Vec<u8>`, never `Frame`.
        decode_frame(&bytes).ok()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl Drop for SimConnection {
    fn drop(&mut self) {
        self.shared.forget_stream(self.local);
    }
}

#[cfg(test)]
mod tests;
