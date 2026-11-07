//! In-memory simulated `SignalingNetwork` (port of
//! `SignalingNetwork.simulated.ts`).
//!
//! Routes datagrams by destination `SocketAddr` through an in-process table —
//! no real sockets. Each `send_to` spawns a detached task that sleeps the
//! configured transit delay then delivers into the destination endpoint's
//! queue, preserving fire-and-forget UDP semantics (send returns immediately;
//! the packet arrives later). This is the fake-transport fabric: a scenario
//! runs in zero wall-time under a paused tokio clock.
//!
//! Per-fiber partition gating belongs with the cluster harness, not here.
//! `send_fault` (per-pair send failure injection) lives here, as it's a
//! pure-network fault primitive.
//!
//! A shared address ([`SimulatedSignalingNetwork::share_address`]) is an
//! address several endpoints serve, each sender's flow delivered to one of
//! them, as an ECMP next hop spreads flows.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use layer_harness::time::now_ms;
use sip_clock::Clock;
use sip_message::Bytes;
use sip_net::Arrival;

use crate::net::{Counters, SignalingNetwork, UdpEndpoint};
use crate::queue::PacketQueue;
use crate::types::{
    BindError, BindErrorReason, BindUdpOpts, PreIngressAction, PreIngressHook, SendError,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// The largest UDP payload over IPv4: past it a real socket refuses the send.
const MAX_DATAGRAM: usize = 65_507;

/// Optional per-pair send-failure injector: `(src, dst) -> Some(reason)` fails
/// that send synchronously (the TS `sendFault`).
pub type SendFault = Arc<dyn Fn(SocketAddr, SocketAddr) -> Option<String> + Send + Sync>;

/// The flow picker of a shared address: `src -> slot` names the member a
/// datagram from `src` is delivered to (see
/// [`SimulatedSignalingNetwork::share_address`]).
pub type FlowPick = Arc<dyn Fn(SocketAddr) -> usize + Send + Sync>;

struct BoundEndpoint {
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    pre_ingress: Option<PreIngressHook>,
    /// This bind's arrival-stamp timeline (see [`BindUdpOpts::clock`]).
    clock: Clock,
}

/// What an address routes to: one endpoint, or the members of a shared
/// address and the picker spreading senders' flows over them.
enum Route {
    One(BoundEndpoint),
    Shared { pick: FlowPick, members: Vec<Option<BoundEndpoint>> },
}

impl Route {
    /// The endpoint a datagram from `src` reaches: the picked member of a
    /// shared address, `None` when that member is not bound.
    fn target(&self, src: SocketAddr) -> Option<&BoundEndpoint> {
        match self {
            Route::One(b) => Some(b),
            Route::Shared { pick, members } => members.get(pick(src)).and_then(Option::as_ref),
        }
    }

    /// Every endpoint bound at the address.
    fn endpoints(&self) -> Box<dyn Iterator<Item = &BoundEndpoint> + '_> {
        match self {
            Route::One(b) => Box::new(std::iter::once(b)),
            Route::Shared { members, .. } => Box::new(members.iter().flatten()),
        }
    }
}

struct SimShared {
    routing: Mutex<HashMap<SocketAddr, Route>>,
    undeliverable: Mutex<Vec<UndeliveredPacket>>,
    in_flight: AtomicI64,
    transit_delay_ms: u64,
    send_fault: Option<SendFault>,
}

/// The simulated network. Clone shares the routing fabric.
#[derive(Clone)]
pub struct SimulatedSignalingNetwork {
    shared: Arc<SimShared>,
}

impl SimulatedSignalingNetwork {
    /// Build a fabric with the given per-hop transit delay (ms).
    ///
    /// A request for `0` is coerced to `1`. Zero transit under a paused runtime
    /// is a determinism trap: delivery is a spawned `sleep(0)` task that races
    /// the multi-stage pipeline (txn actor → router → dispatcher → net), so a
    /// response is processed a turn late (during the next `advance`) and a timer
    /// cancel can land after the timer already fired. A non-zero delay makes
    /// every `recv` park the runtime and auto-advance deterministically. There
    /// is no upside to zero for timer/paused tests, so it is forbidden here.
    pub fn new(transit_delay_ms: u64) -> Self {
        let transit_delay_ms = transit_delay_ms.max(1);
        Self {
            shared: Arc::new(SimShared {
                routing: Mutex::new(HashMap::new()),
                undeliverable: Mutex::new(Vec::new()),
                in_flight: AtomicI64::new(0),
                transit_delay_ms,
                send_fault: None,
            }),
        }
    }

    /// Install a per-pair send-fault injector.
    pub fn with_send_fault(mut self, fault: SendFault) -> Self {
        // The fabric is freshly built here (single owner), so unwrap is safe.
        Arc::get_mut(&mut self.shared)
            .expect("with_send_fault must run before the fabric is shared")
            .send_fault = Some(fault);
        self
    }

    /// Make `addr` an address several endpoints serve, each sender's flow
    /// delivered to one of them, as an ECMP next hop spreads flows. From now
    /// on every bind at `addr` joins it as a member, taking the lowest free
    /// slot (the first bind is member 0); a datagram to `addr` from `src` is
    /// delivered to member `pick(src)` and is undeliverable when that member
    /// is not bound; a member sends with source `addr`; dropping a member
    /// frees only its slot. Runs on any clone. Panics when `addr` is already
    /// bound alone.
    pub fn share_address(&self, addr: SocketAddr, pick: FlowPick) {
        let mut routing = self.shared.routing.lock().unwrap();
        match routing.get_mut(&addr) {
            None => {
                routing.insert(addr, Route::Shared { pick, members: Vec::new() });
            }
            Some(Route::Shared { pick: current, .. }) => *current = pick,
            Some(Route::One(_)) => panic!("{addr} is already bound alone"),
        }
    }
}

#[async_trait]
impl SignalingNetwork for SimulatedSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        let addr = opts.addr;
        let queue = Arc::new(PacketQueue::new(opts.queue_max));
        let counters = Arc::new(Counters::default());

        let bound = BoundEndpoint {
            queue: queue.clone(),
            counters: counters.clone(),
            pre_ingress: opts.pre_ingress.clone(),
            clock: opts.clock.clone(),
        };
        let member = {
            let mut routing = self.shared.routing.lock().unwrap();
            match routing.get_mut(&addr) {
                None => {
                    routing.insert(addr, Route::One(bound));
                    None
                }
                Some(Route::Shared { members, .. }) => {
                    let slot = members.iter().position(Option::is_none).unwrap_or(members.len());
                    if slot == members.len() {
                        members.push(None);
                    }
                    members[slot] = Some(bound);
                    Some(slot)
                }
                Some(Route::One(_)) => {
                    return Err(BindError {
                        reason: BindErrorReason::AlreadyBound,
                        addr,
                        message: format!("already bound: {addr}"),
                    });
                }
            }
        };

        Ok(Box::new(SimEndpoint {
            addr,
            member,
            queue,
            counters,
            queue_max: opts.queue_max,
            shared: self.shared.clone(),
        }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        std::mem::take(&mut *self.shared.undeliverable.lock().unwrap())
    }

    fn transit_delay_ms(&self) -> Option<u64> {
        Some(self.shared.transit_delay_ms)
    }

    fn in_flight(&self) -> i64 {
        self.shared.in_flight.load(Ordering::Relaxed)
    }

    fn bump_in_flight(&self, delta: i64) {
        self.shared.in_flight.fetch_add(delta, Ordering::Relaxed);
    }

    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        self.shared
            .routing
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(addr, route)| route.endpoints().map(|b| (*addr, b.queue.depth())))
            .collect()
    }

    async fn await_in_flight(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.shared.in_flight.load(Ordering::Relaxed) > 0 {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Deliver one datagram into the destination endpoint's queue, applying its
/// pre-ingress hook. Boxed-recursive because the `Reply` branch spawns a
/// follow-up `deliver` (reply travels dst→src after another transit hop).
fn deliver(shared: Arc<SimShared>, raw: Vec<u8>, src: SocketAddr, dst: SocketAddr) -> BoxFut {
    Box::pin(async move {
        let target = {
            let routing = shared.routing.lock().unwrap();
            routing.get(&dst).and_then(|r| r.target(src)).map(|b| {
                (b.queue.clone(), b.counters.clone(), b.pre_ingress.clone(), b.clock.clone())
            })
        };
        let (queue, counters, pre, clock) = match target {
            Some(t) => t,
            None => {
                shared.undeliverable.lock().unwrap().push(UndeliveredPacket {
                    raw,
                    src,
                    dst,
                    timestamp_ms: now_ms(),
                });
                return;
            }
        };

        let depth = queue.depth();
        let mut arrival = Arrival::new(Bytes::from(raw));
        match crate::pre_ingress::judge(pre.as_ref(), &mut arrival, src, depth, &counters) {
            PreIngressAction::Drop => {
                counters.pre_ingress_dropped.fetch_add(1, Ordering::Relaxed);
            }
            PreIngressAction::Reply { datagram, to } => {
                counters.pre_ingress_replies.fetch_add(1, Ordering::Relaxed);
                shared.in_flight.fetch_add(1, Ordering::Relaxed);
                let shared2 = shared.clone();
                let delay = shared.transit_delay_ms;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    deliver(shared2.clone(), datagram, dst, to).await;
                    shared2.in_flight.fetch_sub(1, Ordering::Relaxed);
                });
            }
            PreIngressAction::Accept => {
                let pkt = arrival.into_packet(src, clock.now_ms().max(0) as u64);
                if queue.offer(pkt) {
                    counters.enqueued.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.tail_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })
}

struct SimEndpoint {
    addr: SocketAddr,
    /// This endpoint's slot when `addr` is a shared address.
    member: Option<usize>,
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    queue_max: usize,
    shared: Arc<SimShared>,
}

#[async_trait]
impl UdpEndpoint for SimEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if buf.len() > MAX_DATAGRAM {
            return Err(SendError {
                message: format!("datagram of {} bytes exceeds {MAX_DATAGRAM}", buf.len()),
                kind: sip_net::SendErrorKind::MessageTooLong,
            });
        }
        if let Some(fault) = &self.shared.send_fault {
            if let Some(reason) = fault(self.addr, dst) {
                return Err(SendError::stated(reason));
            }
        }
        self.shared.in_flight.fetch_add(1, Ordering::Relaxed);
        let shared = self.shared.clone();
        let raw = buf.to_vec();
        let src = self.addr;
        let delay = self.shared.transit_delay_ms;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            deliver(shared.clone(), raw, src, dst).await;
            shared.in_flight.fetch_sub(1, Ordering::Relaxed);
        });
        Ok(())
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.queue.take().await
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        self.queue.poll()
    }

    fn peek_queued(&self) -> Vec<UdpPacket> {
        self.queue.peek_all()
    }

    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    fn queue_depth(&self) -> usize {
        self.queue.depth()
    }

    fn queue_max(&self) -> usize {
        self.queue_max
    }

    fn counters(&self) -> UdpEndpointCounters {
        self.counters.snapshot()
    }

    fn install_recv_tap(&self, tap: crate::types::RecvTap) -> bool {
        self.queue.install_tap(tap);
        true
    }
}

impl Drop for SimEndpoint {
    fn drop(&mut self) {
        let mut routing = self.shared.routing.lock().unwrap();
        match (self.member, routing.get_mut(&self.addr)) {
            (Some(slot), Some(Route::Shared { members, .. })) => members[slot] = None,
            _ => {
                routing.remove(&self.addr);
            }
        }
        drop(routing);
        self.queue.close();
    }
}
