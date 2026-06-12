//! [`ProxyCore`] — the stateless proxy data path (port of `ProxyCore.ts`,
//! single-endpoint K8s-LB mode; the dual-fabric registrar mode is out of scope).
//!
//! It binds one UDP endpoint, runs a recv loop, parses each datagram, and
//! dispatches to [`request`](self) / [`response`](self) handling. It owns the
//! routing-policy seam ([`RoutingStrategy`]), the worker registry, the
//! `(Call-ID|CSeq#)` LRU, an [`IdGen`] for Via branches, a [`Clock`], metrics,
//! a logger, and the (stubbed) self-gate. The handlers mutate the raw header
//! list and re-serialize — `sip-message::serialize` renders from `headers`, so
//! Via/Record-Route/Route surgery takes effect directly.

mod request;
mod response;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_txn::IdGen;

use crate::addr::ProxyAddr;
use crate::cancel_lru::CancelBranchLru;
use crate::observability::logger::{NoopLogger, ProxyLogger};
use crate::observability::ProxyMetrics;
use crate::registry::WorkerRegistry;
use crate::resolver::{HostResolver, NamedForwarder, ResolverConfig, SystemResolver};
use crate::self_gate::{AlwaysAdmitGate, ProxySelfGate};
use crate::strategy::RoutingStrategy;

/// Methods that create a dialog (RFC 3261) — the proxy inserts a Record-Route
/// only for these.
fn is_dialog_creating(method: &str) -> bool {
    method == "INVITE" || method == "SUBSCRIBE"
}

/// The dependency bundle for a [`ProxyCore`] (avoids a 10-argument constructor).
pub struct ProxyCoreParts {
    pub endpoint: Box<dyn UdpEndpoint>,
    /// The address the proxy stamps on Via / Record-Route (its advertised
    /// `host:port`). Usually the endpoint's bound address.
    pub advertised: ProxyAddr,
    pub strategy: Arc<dyn RoutingStrategy>,
    pub registry: Arc<dyn WorkerRegistry>,
    pub cancel_lru: Arc<CancelBranchLru>,
    pub id_gen: Arc<IdGen>,
    pub clock: Clock,
    pub metrics: Arc<ProxyMetrics>,
    pub logger: Arc<dyn ProxyLogger>,
    pub self_gate: Arc<dyn ProxySelfGate>,
    /// Resolver for DNS-named forward targets (worker-outbound b-leg R-URIs).
    /// IP-literal traffic never touches it; see [`crate::resolver`].
    pub resolver: Arc<dyn HostResolver>,
}

/// The stateless proxy.
pub struct ProxyCore {
    endpoint: Arc<dyn UdpEndpoint>,
    advertised: ProxyAddr,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    cancel_lru: Arc<CancelBranchLru>,
    id_gen: Arc<IdGen>,
    clock: Clock,
    metrics: Arc<ProxyMetrics>,
    logger: Arc<dyn ProxyLogger>,
    self_gate: Arc<dyn ProxySelfGate>,
    parser: CustomParser,
    /// Off-loop send path for DNS-named targets (cache + single-flight resolve).
    named: NamedForwarder,
}

impl ProxyCore {
    pub fn new(parts: ProxyCoreParts) -> Self {
        let endpoint: Arc<dyn UdpEndpoint> = Arc::from(parts.endpoint);
        let named = NamedForwarder::new(
            endpoint.clone(),
            parts.resolver,
            ResolverConfig::default(),
            parts.clock.clone(),
            parts.metrics.clone(),
        );
        Self {
            endpoint,
            advertised: parts.advertised,
            strategy: parts.strategy,
            registry: parts.registry,
            cancel_lru: parts.cancel_lru,
            id_gen: parts.id_gen,
            clock: parts.clock,
            metrics: parts.metrics,
            logger: parts.logger,
            self_gate: parts.self_gate,
            parser: CustomParser::default(),
            named,
        }
    }

    /// A handle to this proxy's metrics (for SUT assertions / a metrics server).
    pub fn metrics(&self) -> Arc<ProxyMetrics> {
        self.metrics.clone()
    }

    pub fn advertised(&self) -> &ProxyAddr {
        &self.advertised
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Send raw bytes to a [`ProxyAddr`] target. An IP-literal host (the fast
    /// path: registry workers and the simulated fabric are always IP literals)
    /// is sent inline; a DNS name — e.g. a `sipp-uas` pod FQDN in a
    /// worker-outbound b-leg R-URI — goes through the [`NamedForwarder`], which
    /// serves cache hits inline and resolves misses on a spawned task so the
    /// recv loop NEVER waits on DNS (see [`crate::resolver`]).
    async fn send_to(&self, bytes: &[u8], target: &ProxyAddr) {
        if let Some(dst) = target.to_socket_addr() {
            if self.endpoint.send_to(bytes, dst).await.is_err() {
                self.metrics.record_send_failure();
            }
            return;
        }
        self.named.send(bytes, target).await;
    }

    /// Reply to the packet's source.
    async fn reply_to_source(&self, bytes: &[u8], src: SocketAddr) {
        if self.endpoint.send_to(bytes, src).await.is_err() {
            self.metrics.record_send_failure();
        }
    }

    /// The recv loop. Runs until the endpoint's queue is closed (the endpoint
    /// is dropped). Parse failures are dropped silently (RFC 3261 §16.3).
    pub async fn run(self) {
        // Background sweeper for the `(Call-ID|CSeq#)` LRU. `lookup` only evicts
        // an entry when it is looked up *after* expiry — which an answered (2xx)
        // call never does (no CANCEL, no proxy-absorbed ACK), so without this
        // task the map (and `sip_proxy_pending_invite_lru_size`) grows ≈ the
        // cumulative-INVITE count for the life of the process. Sweeping every
        // half-TTL physically reclaims expired slots and re-publishes the gauge,
        // pinning the map at ~1× working set. See [`crate::cancel_lru`].
        let sweeper = {
            let lru = self.cancel_lru.clone();
            let metrics = self.metrics.clone();
            let endpoint = self.endpoint.clone();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(Duration::from_millis(crate::cancel_lru::DEFAULT_SWEEP_INTERVAL_MS));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    lru.sweep_expired();
                    metrics.set_pending_invite_lru_size(lru.size() as u64);
                    // Publish the endpoint's queue state: a tail-dropping
                    // queue otherwise shows 100% forwarded on dashboards.
                    let c = endpoint.counters();
                    metrics.set_udp_endpoint_stats(
                        endpoint.queue_depth() as u64,
                        endpoint.queue_max() as u64,
                        c.enqueued,
                        c.tail_dropped,
                    );
                }
            })
        };

        while let Some(pkt) = self.endpoint.recv().await {
            let Ok(msg) = self.parser.parse(&pkt.raw) else {
                // Malformed datagram — drop silently.
                continue;
            };
            match msg {
                SipMessage::Request(req) => self.handle_request(req, pkt.src).await,
                SipMessage::Response(resp) => self.handle_response(resp).await,
            }
        }

        // Endpoint closed — stop the sweeper so it doesn't outlive the core.
        sweeper.abort();
    }
}

/// Convenience builder for the common all-defaults wiring (NoopLogger +
/// AlwaysAdmitGate + fresh metrics + entropy IdGen + system clock).
pub struct ProxyCoreBuilder {
    advertised: ProxyAddr,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    cancel_lru: Option<Arc<CancelBranchLru>>,
    id_gen: Option<Arc<IdGen>>,
    clock: Option<Clock>,
    metrics: Option<Arc<ProxyMetrics>>,
    logger: Option<Arc<dyn ProxyLogger>>,
    self_gate: Option<Arc<dyn ProxySelfGate>>,
    resolver: Option<Arc<dyn HostResolver>>,
}

impl ProxyCoreBuilder {
    pub fn new(advertised: ProxyAddr, strategy: Arc<dyn RoutingStrategy>, registry: Arc<dyn WorkerRegistry>) -> Self {
        Self {
            advertised,
            strategy,
            registry,
            cancel_lru: None,
            id_gen: None,
            clock: None,
            metrics: None,
            logger: None,
            self_gate: None,
            resolver: None,
        }
    }

    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }
    pub fn id_gen(mut self, id_gen: Arc<IdGen>) -> Self {
        self.id_gen = Some(id_gen);
        self
    }
    pub fn metrics(mut self, metrics: Arc<ProxyMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
    pub fn logger(mut self, logger: Arc<dyn ProxyLogger>) -> Self {
        self.logger = Some(logger);
        self
    }
    pub fn self_gate(mut self, gate: Arc<dyn ProxySelfGate>) -> Self {
        self.self_gate = Some(gate);
        self
    }
    pub fn cancel_lru(mut self, lru: Arc<CancelBranchLru>) -> Self {
        self.cancel_lru = Some(lru);
        self
    }
    pub fn resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Finish into a [`ProxyCore`] bound on `endpoint`.
    pub fn build(self, endpoint: Box<dyn UdpEndpoint>) -> ProxyCore {
        let clock = self.clock.unwrap_or_else(Clock::system);
        ProxyCore::new(ProxyCoreParts {
            endpoint,
            advertised: self.advertised,
            strategy: self.strategy,
            registry: self.registry,
            cancel_lru: self.cancel_lru.unwrap_or_else(|| Arc::new(CancelBranchLru::with_clock(clock.clone()))),
            id_gen: self.id_gen.unwrap_or_else(|| Arc::new(IdGen::from_entropy())),
            clock,
            metrics: self.metrics.unwrap_or_else(|| Arc::new(ProxyMetrics::new())),
            logger: self.logger.unwrap_or_else(|| Arc::new(NoopLogger)),
            self_gate: self.self_gate.unwrap_or_else(|| Arc::new(AlwaysAdmitGate)),
            resolver: self.resolver.unwrap_or_else(|| Arc::new(SystemResolver)),
        })
    }
}

#[cfg(test)]
mod sweeper_tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    use async_trait::async_trait;
    use sip_net::{SendError, UdpEndpointCounters, UdpPacket};

    use crate::cancel_lru::{call_id_cseq_key, CancelEntry, RTX_ENTRY_TTL_MS};
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::ForwardAllStrategy;

    /// A `UdpEndpoint` whose `recv` never resolves: the proxy's recv loop parks,
    /// so the background sweeper is the only task making progress.
    struct PendingEndpoint;

    #[async_trait]
    impl UdpEndpoint for PendingEndpoint {
        async fn send_to(&self, _buf: &[u8], _dst: SocketAddr) -> Result<(), SendError> {
            Ok(())
        }
        async fn recv(&self) -> Option<UdpPacket> {
            std::future::pending::<Option<UdpPacket>>().await
        }
        fn try_recv(&self) -> Option<UdpPacket> {
            None
        }
        fn local_addr(&self) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
        }
        fn queue_depth(&self) -> usize {
            0
        }
        fn queue_max(&self) -> usize {
            0
        }
        fn counters(&self) -> UdpEndpointCounters {
            UdpEndpointCounters::default()
        }
    }

    /// Regression: a running core's background sweeper physically reclaims an
    /// expired pending-INVITE entry and re-publishes the gauge to match. Without
    /// the sweeper the entry would linger for the life of the process (an
    /// answered call never re-`lookup`s its key, so lazy eviction never fires)
    /// and `sip_proxy_pending_invite_lru_size` would climb without bound.
    #[tokio::test(start_paused = true)]
    async fn run_sweeps_expired_pending_invite_entries() {
        // (Cadence ≤ shortest TTL is a compile-time invariant in cancel_lru.rs.)
        let clock = Clock::test_at(0);
        let lru = Arc::new(CancelBranchLru::with_clock(clock.clone()));
        let metrics = Arc::new(ProxyMetrics::new());

        // Remember one entry at the SHORT (rtx) TTL and publish the gauge, as
        // the request path does on every forward.
        lru.remember(
            &call_id_cseq_key("call-leaky", Some("t"), 1),
            CancelEntry { target: ProxyAddr::new("10.0.0.2", 5070), branch: "z9hG4bK-x".into() },
            RTX_ENTRY_TTL_MS,
        );
        metrics.set_pending_invite_lru_size(lru.size() as u64);
        assert_eq!(metrics.pending_invite_lru_size(), 1);

        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new("10.0.0.2", 5070)));
        let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new("127.0.0.1", 5060), strategy, registry)
            .clock(clock)
            .cancel_lru(lru.clone())
            .metrics(metrics.clone())
            .build(Box::new(PendingEndpoint));
        let task = tokio::spawn(core.run());

        // Advance past the 32 s TTL so a sweep tick lands after the entry expires;
        // the map must drain and the gauge follow it down.
        for _ in 0..50 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
            if lru.size() == 0 {
                break;
            }
        }

        assert_eq!(lru.size(), 0, "sweeper must physically reclaim the expired entry");
        assert_eq!(metrics.pending_invite_lru_size(), 0, "gauge must follow the reclaimed map down");

        task.abort();
    }
}
