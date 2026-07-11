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
//! Deferred vs. the source (tracked in MIGRATION_STATUS): the `ConnectivityGate`
//! (per-fiber partition gating for the k8s reliability tests) is not ported in
//! this slice — it belongs with the cluster harness. `send_fault` (per-pair
//! send failure injection) is ported, as it's a pure-network fault primitive.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use layer_harness::time::now_ms;

use crate::net::{Counters, SignalingNetwork, UdpEndpoint};
use crate::queue::PacketQueue;
use crate::types::{
    BindError, BindErrorReason, BindUdpOpts, PreIngressAction, PreIngressHook, SendError,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// Optional per-pair send-failure injector: `(src, dst) -> Some(reason)` fails
/// that send synchronously (the TS `sendFault`).
pub type SendFault = Arc<dyn Fn(SocketAddr, SocketAddr) -> Option<String> + Send + Sync>;

struct BoundEndpoint {
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    pre_ingress: Option<PreIngressHook>,
}

struct SimShared {
    routing: Mutex<HashMap<SocketAddr, BoundEndpoint>>,
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
}

#[async_trait]
impl SignalingNetwork for SimulatedSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        let addr = opts.addr;
        let queue = Arc::new(PacketQueue::new(opts.queue_max));
        let counters = Arc::new(Counters::default());

        {
            let mut routing = self.shared.routing.lock().unwrap();
            if routing.contains_key(&addr) {
                return Err(BindError {
                    reason: BindErrorReason::AlreadyBound,
                    addr,
                    message: format!("already bound: {addr}"),
                });
            }
            routing.insert(
                addr,
                BoundEndpoint {
                    queue: queue.clone(),
                    counters: counters.clone(),
                    pre_ingress: opts.pre_ingress.clone(),
                },
            );
        }

        Ok(Box::new(SimEndpoint {
            addr,
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
            .map(|(addr, b)| (*addr, b.queue.depth()))
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
            routing
                .get(&dst)
                .map(|b| (b.queue.clone(), b.counters.clone(), b.pre_ingress.clone()))
        };
        let (queue, counters, pre) = match target {
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
        let action = match &pre {
            Some(hook) => hook(&raw, src, depth),
            None => PreIngressAction::Accept,
        };
        match action {
            PreIngressAction::Drop => {
                counters.pre_ingress_dropped.fetch_add(1, Ordering::Relaxed);
            }
            PreIngressAction::Reply(bytes) => {
                counters.pre_ingress_replies.fetch_add(1, Ordering::Relaxed);
                shared.in_flight.fetch_add(1, Ordering::Relaxed);
                let shared2 = shared.clone();
                let delay = shared.transit_delay_ms;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    deliver(shared2.clone(), bytes, dst, src).await;
                    shared2.in_flight.fetch_sub(1, Ordering::Relaxed);
                });
            }
            PreIngressAction::Accept => {
                let pkt = UdpPacket {
                    raw,
                    src,
                    arrival_ms: now_ms(),
                };
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
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    queue_max: usize,
    shared: Arc<SimShared>,
}

#[async_trait]
impl UdpEndpoint for SimEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if let Some(fault) = &self.shared.send_fault {
            if let Some(reason) = fault(self.addr, dst) {
                return Err(SendError { message: reason });
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
        self.shared.routing.lock().unwrap().remove(&self.addr);
        self.queue.close();
    }
}
