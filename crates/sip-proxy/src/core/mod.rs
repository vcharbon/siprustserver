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
}

/// The stateless proxy.
pub struct ProxyCore {
    endpoint: Box<dyn UdpEndpoint>,
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
}

impl ProxyCore {
    pub fn new(parts: ProxyCoreParts) -> Self {
        Self {
            endpoint: parts.endpoint,
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

    /// Send raw bytes to a [`ProxyAddr`] target (resolving to a socket addr).
    /// A target whose host isn't an IP literal is dropped (logged via metrics
    /// elsewhere) — the simulated fabric + tests always address by IP literal.
    async fn send_to(&self, bytes: &[u8], target: &ProxyAddr) {
        if let Some(dst) = target.to_socket_addr() {
            let _ = self.endpoint.send_to(bytes, dst).await;
        }
    }

    /// Reply to the packet's source.
    async fn reply_to_source(&self, bytes: &[u8], src: SocketAddr) {
        let _ = self.endpoint.send_to(bytes, src).await;
    }

    /// The recv loop. Runs until the endpoint's queue is closed (the endpoint
    /// is dropped). Parse failures are dropped silently (RFC 3261 §16.3).
    pub async fn run(self) {
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

    /// Finish into a [`ProxyCore`] bound on `endpoint`.
    pub fn build(self, endpoint: Box<dyn UdpEndpoint>) -> ProxyCore {
        let clock = self.clock.unwrap_or_else(Clock::system);
        ProxyCore::new(ProxyCoreParts {
            endpoint,
            advertised: self.advertised,
            strategy: self.strategy,
            registry: self.registry,
            cancel_lru: self
                .cancel_lru
                .unwrap_or_else(|| Arc::new(CancelBranchLru::with_opts(crate::cancel_lru::DEFAULT_TTL_MS, clock.clone()))),
            id_gen: self.id_gen.unwrap_or_else(|| Arc::new(IdGen::from_entropy())),
            clock,
            metrics: self.metrics.unwrap_or_else(|| Arc::new(ProxyMetrics::new())),
            logger: self.logger.unwrap_or_else(|| Arc::new(NoopLogger)),
            self_gate: self.self_gate.unwrap_or_else(|| Arc::new(AlwaysAdmitGate)),
        })
    }
}
